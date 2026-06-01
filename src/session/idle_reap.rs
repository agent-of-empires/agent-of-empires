//! Pure decision logic for auto-stopping idle plain TUI/tmux sessions
//! (`session.auto_stop_idle_secs`, #1690).
//!
//! This module owns the eligibility predicate only: no tmux calls, no storage
//! writes, no config resolution, no process locks. Callers (the TUI main loop
//! and the serve `status_poll_loop`) resolve the per-profile threshold, gather
//! the live tmux attach state, then ask this predicate per session and claim
//! the stop through `Storage::update` so concurrent reapers cannot double-stop.
//!
//! The cockpit substrate has its own reaper (`server::cockpit_reconciler`,
//! #1689) with dormancy and seamless respawn; this is the tmux-substrate analog
//! where a stop kills the pane and leaves a restartable `Stopped` row.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use super::{Instance, Status, Storage};

/// A plain session the reaper intends to auto-stop, with the inputs the
/// caller needs to claim it (`profile` to open the right storage, the resolved
/// `threshold_secs` for the in-lock re-check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleReapCandidate {
    pub session_id: String,
    pub profile: String,
    pub threshold_secs: u32,
}

/// Select the plain (non-cockpit) sessions eligible for idle auto-stop.
///
/// Shared by the TUI main loop and the serve `status_poll_loop` so both apply
/// identical policy. `attached` is the set of tmux session names with a live
/// client (from [`crate::tmux::attached_session_names`]); a session whose tmux
/// name is in it is spared. `resolve_threshold` maps a profile name to its
/// effective `session.auto_stop_idle_secs`; callers cache it per profile.
pub fn idle_reap_candidates(
    instances: &[Instance],
    now: DateTime<Utc>,
    attached: &HashSet<String>,
    resolve_threshold: impl Fn(&str) -> u32,
) -> Vec<IdleReapCandidate> {
    let mut candidates = Vec::new();
    for inst in instances {
        if inst.is_cockpit_mode() {
            continue;
        }
        let profile = inst.effective_profile();
        let threshold_secs = resolve_threshold(&profile);
        if threshold_secs == 0 {
            continue;
        }
        let is_attached = inst
            .tmux_session()
            .ok()
            .is_some_and(|s| attached.contains(s.name()));
        if should_auto_stop_session(
            now,
            inst.status,
            inst.idle_entered_at,
            inst.last_accessed_at,
            is_attached,
            threshold_secs,
        ) {
            candidates.push(IdleReapCandidate {
                session_id: inst.id.clone(),
                profile,
                threshold_secs,
            });
        }
    }
    candidates
}

/// Decide whether a plain (non-cockpit) session should be auto-stopped for
/// inactivity.
///
/// Eligible only when all hold:
/// - `threshold_secs > 0` (the feature is opt-in; `0` disables it),
/// - the session is currently `Idle` (a `Running`/`Waiting` session is never
///   stopped, which `idle_entered_at` also reflects since it is cleared on any
///   non-Idle transition),
/// - no tmux client is attached (a session the user is reading is spared),
/// - `idle_entered_at` is known (legacy rows that predate the field are
///   skipped rather than inferred),
/// - the idle anchor is at least `threshold_secs` in the past.
///
/// The anchor is `max(idle_entered_at, last_accessed_at)`: `last_accessed_at`
/// is bumped on user interaction (and equals `idle_entered_at` at the moment a
/// session enters `Idle`), so a session the user recently touched is spared
/// even if the agent went idle earlier. A negative elapsed (clock skew, anchor
/// in the future) is treated as not-yet-eligible rather than panicking.
pub fn should_auto_stop_session(
    now: DateTime<Utc>,
    status: Status,
    idle_entered_at: Option<DateTime<Utc>>,
    last_accessed_at: Option<DateTime<Utc>>,
    is_attached: bool,
    threshold_secs: u32,
) -> bool {
    if threshold_secs == 0 {
        return false;
    }
    if status != Status::Idle {
        return false;
    }
    if is_attached {
        return false;
    }
    let Some(entered) = idle_entered_at else {
        return false;
    };
    let anchor = match last_accessed_at {
        Some(accessed) if accessed > entered => accessed,
        _ => entered,
    };
    match (now - anchor).to_std() {
        Ok(elapsed) => elapsed.as_secs() >= threshold_secs as u64,
        // Negative duration (anchor in the future / clock skew): not eligible.
        Err(_) => false,
    }
}

/// Atomically claim an idle session for auto-stop, under the per-profile
/// storage file lock so concurrent reapers (a standalone TUI and an `aoe serve`
/// daemon against the same on-disk state) cannot double-stop it.
///
/// Re-reads the session from disk inside the lock and re-checks eligibility
/// (still `Idle`, still idle past `threshold_secs`); attach state is not
/// re-queried here because it is a blocking tmux call and the caller already
/// filtered attached sessions before claiming. On success the on-disk status
/// is flipped to `Stopped` (the claim, mirroring the manual-stop path) and the
/// claimed `Instance` is returned for the caller to actually kill via
/// `perform_stop`. Returns `Ok(None)` when the session is no longer eligible
/// (already claimed by the peer reaper, woken by the user, or gone).
pub fn claim_idle_stop(
    profile: &str,
    session_id: &str,
    now: DateTime<Utc>,
    threshold_secs: u32,
) -> anyhow::Result<Option<Instance>> {
    let storage = Storage::new(profile)?;
    storage.update(|instances, _groups| {
        let Some(inst) = instances.iter_mut().find(|i| i.id == session_id) else {
            return Ok(None);
        };
        // Defense in depth: never stop a cockpit row through the plain-session
        // path, even if a caller reached here without going through
        // `idle_reap_candidates` (which already excludes cockpit sessions).
        if inst.is_cockpit_mode() {
            return Ok(None);
        }
        let eligible = should_auto_stop_session(
            now,
            inst.status,
            inst.idle_entered_at,
            inst.last_accessed_at,
            false,
            threshold_secs,
        );
        if !eligible {
            return Ok(None);
        }
        inst.status = Status::Stopped;
        Ok(Some(inst.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn disabled_threshold_never_stops() {
        let n = now();
        assert!(!should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::hours(10)),
            None,
            false,
            0,
        ));
    }

    #[test]
    fn non_idle_is_never_stopped() {
        let n = now();
        for status in [Status::Running, Status::Waiting, Status::Error] {
            assert!(
                !should_auto_stop_session(
                    n,
                    status,
                    Some(n - Duration::hours(10)),
                    None,
                    false,
                    60,
                ),
                "status {status:?} should survive the reap"
            );
        }
    }

    #[test]
    fn attached_session_is_never_stopped() {
        let n = now();
        assert!(!should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::hours(10)),
            None,
            true,
            60,
        ));
    }

    #[test]
    fn missing_idle_entered_at_never_stops() {
        let n = now();
        assert!(!should_auto_stop_session(
            n,
            Status::Idle,
            None,
            None,
            false,
            60,
        ));
    }

    #[test]
    fn idle_past_threshold_stops() {
        let n = now();
        assert!(should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::seconds(120)),
            None,
            false,
            60,
        ));
    }

    #[test]
    fn idle_within_threshold_survives() {
        let n = now();
        assert!(!should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::seconds(30)),
            None,
            false,
            60,
        ));
    }

    #[test]
    fn exactly_at_threshold_stops() {
        let n = now();
        assert!(should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::seconds(60)),
            None,
            false,
            60,
        ));
    }

    #[test]
    fn recent_access_after_idle_entry_spares_session() {
        let n = now();
        // Went idle 2h ago, but the user interacted 10s ago: anchor is the
        // recent access, so the session is spared.
        assert!(!should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::hours(2)),
            Some(n - Duration::seconds(10)),
            false,
            60,
        ));
    }

    #[test]
    fn stale_access_does_not_extend_idle() {
        let n = now();
        // last_accessed_at older than idle entry: anchor stays at idle entry,
        // which is past the threshold.
        assert!(should_auto_stop_session(
            n,
            Status::Idle,
            Some(n - Duration::seconds(120)),
            Some(n - Duration::hours(5)),
            false,
            60,
        ));
    }

    #[test]
    fn future_anchor_clock_skew_does_not_stop() {
        let n = now();
        assert!(!should_auto_stop_session(
            n,
            Status::Idle,
            Some(n + Duration::seconds(60)),
            None,
            false,
            60,
        ));
    }

    fn idle_instance(title: &str) -> Instance {
        let mut inst = Instance::new(title, "/tmp/idle-reap-test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - Duration::seconds(120));
        inst
    }

    #[test]
    fn candidates_select_idle_past_threshold() {
        let n = now();
        let instances = vec![idle_instance("a")];
        let attached = HashSet::new();
        let got = idle_reap_candidates(&instances, n, &attached, |_| 60);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_id, instances[0].id);
        assert_eq!(got[0].threshold_secs, 60);
    }

    #[test]
    fn candidates_skip_disabled_threshold() {
        let n = now();
        let instances = vec![idle_instance("a")];
        let attached = HashSet::new();
        assert!(idle_reap_candidates(&instances, n, &attached, |_| 0).is_empty());
    }

    #[test]
    fn candidates_skip_running_session() {
        let n = now();
        let mut inst = idle_instance("a");
        inst.status = Status::Running;
        let attached = HashSet::new();
        assert!(idle_reap_candidates(&[inst], n, &attached, |_| 60).is_empty());
    }

    #[test]
    fn candidates_skip_attached_session() {
        let n = now();
        let inst = idle_instance("a");
        let name = inst.tmux_session().unwrap().name().to_string();
        let mut attached = HashSet::new();
        attached.insert(name);
        assert!(idle_reap_candidates(&[inst], n, &attached, |_| 60).is_empty());
    }

    /// Saves and restores `HOME` / `XDG_CONFIG_HOME` so a test that points the
    /// app dir at a tempdir does not leak that into sibling tests sharing the
    /// process.
    struct EnvGuard {
        home: Option<std::ffi::OsString>,
        xdg: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                home: std::env::var_os("HOME"),
                xdg: std::env::var_os("XDG_CONFIG_HOME"),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match &self.xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn claim_is_single_shot_under_storage_lock() {
        // The double-reap guard: the first claim wins and flips the on-disk
        // status to Stopped; a second claim (the peer reaper) sees a non-Idle
        // session and returns None, so the session is never stopped twice.
        let _env = EnvGuard::capture();
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(target_os = "linux")]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let inst = idle_instance("claimable");
        let id = inst.id.clone();
        let storage = Storage::new("test-profile").unwrap();
        storage
            .update(|instances, _groups| {
                instances.push(inst);
                Ok(())
            })
            .unwrap();

        let now = Utc::now();
        let first = claim_idle_stop("test-profile", &id, now, 60).unwrap();
        assert!(first.is_some(), "first claim should win");

        let second = claim_idle_stop("test-profile", &id, now, 60).unwrap();
        assert!(
            second.is_none(),
            "second claim must not re-stop the session"
        );

        let stored = storage.load().unwrap();
        assert_eq!(stored[0].status, Status::Stopped);
    }
}
