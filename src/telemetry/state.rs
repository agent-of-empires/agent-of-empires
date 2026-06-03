//! Persistence of the anonymous install id.
//!
//! Stored in a dedicated `<app_dir>/telemetry.json`, deliberately separate
//! from `config.toml`: users routinely paste their config into bug reports,
//! and the id leaking there would both expose it and corrupt distinct-install
//! counts. The file is created only on opt-in and deleted on opt-out.

use anyhow::Result;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::get_app_dir;

/// Sidecar advisory-lock file next to `telemetry.json`. Mirrors the `.lock`
/// sidecars used by `src/session/storage.rs` and `src/logging.rs`; left on disk
/// (never removed) so two processes can't race to recreate and re-lock it.
const STATE_LOCK_FILENAME: &str = ".telemetry.lock";

/// How long [`update_state_locked`] will wait for the cross-process flock before
/// giving up. Telemetry is fire-and-forget and must never stall a user command,
/// so a contended lock is abandoned (the update is skipped) rather than waited
/// on; the only loss is a single deferred stamp, recovered on the next call.
const LOCK_ACQUIRE_BUDGET: Duration = Duration::from_millis(500);

/// Poll cadence for the flock acquire. Well below human perception and far
/// above the microsecond hold time of a single tiny-file read-modify-write.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Serializes the `telemetry.json` read-modify-write across threads *within*
/// this process; the file flock serializes it across processes. Always taken
/// before the flock so the ordering can never invert into a deadlock.
static STATE_RMW_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize, Deserialize)]
struct TelemetryState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
    /// Last time a CLI `process_start` was *confirmed delivered*, used to throttle
    /// the one unbounded event source to at most once per install per day. Long-lived
    /// surfaces (TUI / serve) emit once per launch and need no throttle. Only stamped
    /// on a successful send, so a failed send leaves the daily slot open for retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_cli_process_start: Option<DateTime<Utc>>,
    /// Last time a CLI `process_start` send was *attempted* (success or failure).
    /// Bounds retries: while the daily slot is open after a failed send, this stops
    /// every `aoe` invocation from re-attempting against a down endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_cli_process_start_attempt: Option<DateTime<Utc>>,
}

fn state_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("telemetry.json"))
}

fn load_state() -> TelemetryState {
    let Ok(path) = state_path() else {
        return TelemetryState::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return TelemetryState::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_state(state: &TelemetryState) -> Result<()> {
    let path = state_path()?;
    let content = serde_json::to_string_pretty(state)?;
    crate::session::atomic_write(&path, content.as_bytes())?;
    // The id is mildly sensitive (it's the distinct-install key); keep the
    // file owner-only, matching the `aoe serve` runtime files.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// RAII guard for the held cross-process advisory flock. `fs2` (and the kernel
/// on fd close / process exit, including SIGKILL) releases the lock on drop, so
/// a panic in the critical section still frees it for the next process.
struct StateFlock {
    file: std::fs::File,
}

impl Drop for StateFlock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Acquire the exclusive advisory flock on `<app_dir>/.telemetry.lock`, polling
/// `try_lock_exclusive` until granted or [`LOCK_ACQUIRE_BUDGET`] elapses. Bounded
/// (unlike the storage flock's indefinite wait) because fire-and-forget telemetry
/// must never block the caller. User-initiated mutations use
/// [`acquire_state_flock_blocking`] instead.
fn acquire_state_flock() -> Result<StateFlock> {
    let file = open_state_lock_file()?;
    let started = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(StateFlock { file }),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= LOCK_ACQUIRE_BUDGET {
                    anyhow::bail!("telemetry state lock contended beyond budget");
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Acquire the exclusive flock with a *blocking* wait, for user-initiated
/// mutations (opt-out / `reset-id`) whose documented contract is that the file
/// is actually deleted. Unlike [`acquire_state_flock`], it does not give up on
/// contention: the bounded skip is right for fire-and-forget telemetry, but an
/// explicit opt-out must not silently leave `telemetry.json` behind. The kernel
/// releases the lock on fd close / process exit, so a crashed peer can't wedge
/// this forever.
fn acquire_state_flock_blocking() -> Result<StateFlock> {
    let file = open_state_lock_file()?;
    FileExt::lock_exclusive(&file)?;
    Ok(StateFlock { file })
}

/// Open (creating if needed) the sidecar lock file. Shared by the bounded and
/// blocking acquire paths. Open semantics mirror the storage / logging locks
/// (read+write, create, no truncate, `0o600` on Unix).
fn open_state_lock_file() -> Result<std::fs::File> {
    let path = get_app_dir()?.join(STATE_LOCK_FILENAME);
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?
    };
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    Ok(file)
}

/// Run a `telemetry.json` read-modify-write under both the process-local mutex
/// and the cross-process flock, so concurrent updaters (TUI, CLI, `aoe serve`)
/// can never lose each other's writes (the lost-update race in #1877). `f`
/// receives the loaded state and returns `(value, dirty)`; the state is
/// persisted only when `dirty` is true, so a pure check never recreates the
/// file. Returns `Err` (and writes nothing) when the lock is contended past the
/// budget; callers treat that as "skip this update".
fn update_state_locked<R>(f: impl FnOnce(&mut TelemetryState) -> (R, bool)) -> Result<R> {
    let _guard = STATE_RMW_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _flock = acquire_state_flock()?;
    let mut state = load_state();
    let (value, dirty) = f(&mut state);
    if dirty {
        save_state(&state)?;
    }
    Ok(value)
}

/// Whether a CLI `process_start` send is due given an already-loaded `state`.
/// Due when the last *confirmed* send is older than `success_gap` (or never)
/// AND the last *attempt* is older than `retry_gap` (or never). Pure: the
/// caller evaluates it under the lock in [`reserve_cli_process_start`] so the
/// check and the claim are one transaction.
fn due_in_state(
    state: &TelemetryState,
    now: DateTime<Utc>,
    success_gap: Duration,
    retry_gap: Duration,
) -> bool {
    // A stamp is "fresh" when its positive elapsed is still inside the gap. A
    // negative elapsed (clock skew) counts as not fresh, so the send is allowed.
    let fresh = |stamp: Option<DateTime<Utc>>, gap: Duration| match stamp {
        Some(last) => matches!((now - last).to_std(), Ok(elapsed) if elapsed < gap),
        None => false,
    };
    !fresh(state.last_cli_process_start, success_gap)
        && !fresh(state.last_cli_process_start_attempt, retry_gap)
}

/// The current install id, if one has been generated. Read-only: never
/// generates. Returns `None` when telemetry was never opted into.
pub fn install_id() -> Option<String> {
    load_state().install_id.filter(|s| !s.trim().is_empty())
}

/// Return the existing install id, generating and persisting a fresh random
/// UUID v4 if none exists. Honors `DO_NOT_TRACK`: when set, never generates
/// or persists an id and returns `None`.
pub fn ensure_install_id() -> Option<String> {
    if super::do_not_track() {
        return None;
    }
    let result = update_state_locked(|state| {
        if let Some(id) = state.install_id.as_ref().filter(|s| !s.trim().is_empty()) {
            return (Some(id.clone()), false);
        }
        let id = uuid::Uuid::new_v4().to_string();
        state.install_id = Some(id.clone());
        (Some(id), true)
    });
    match result {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(target: "telemetry", "failed to persist install id: {e}");
            None
        }
    }
}

/// Delete the install id (and its file) on opt-out. Idempotent.
pub fn delete_install_id() -> Result<()> {
    // Held under the same mutex + flock as the writers so an opt-out can't race
    // a concurrent `ensure_install_id` and leave a half-state behind.
    let _guard = STATE_RMW_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Blocking acquire, not the fire-and-forget bounded one: an explicit opt-out
    // must delete the file even under contention, per the documented contract.
    let _flock = acquire_state_flock_blocking()?;
    let path = state_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Delete the current id and generate a fresh one. Used by
/// `aoe telemetry reset-id`. Returns the new id, or `None` if suppressed by
/// `DO_NOT_TRACK`.
pub fn reset_install_id() -> Option<String> {
    if let Err(e) = delete_install_id() {
        tracing::debug!(target: "telemetry", "failed to delete install id during reset: {e}");
    }
    ensure_install_id()
}

/// Atomically reserve a CLI `process_start` send, returning the install id to
/// send with when one is *due*, or `None` when not due, suppressed by
/// `DO_NOT_TRACK`, or the lock is contended. Due when the last *confirmed* send
/// is older than `success_gap` (or never) AND the last *attempt* older than
/// `retry_gap` (or never). The whole check-and-claim runs under the lock: the
/// attempt stamp is written and an install id ensured in the same transaction,
/// so two concurrent `aoe` invocations can never both pass the gate and both
/// send (the duplicate-send race in #1877). `success_gap` is the once-per-day
/// throttle that bounds the only high-frequency source; `retry_gap` bounds how
/// often a failed send is retried so a down endpoint can't turn every
/// invocation into a fresh attempt. Caller owns the opt-in gate.
pub fn reserve_cli_process_start(success_gap: Duration, retry_gap: Duration) -> Option<String> {
    if super::do_not_track() {
        return None;
    }
    update_state_locked(|state| {
        let now = Utc::now();
        if !due_in_state(state, now, success_gap, retry_gap) {
            return (None, false);
        }
        let id = match state.install_id.as_ref().filter(|s| !s.trim().is_empty()) {
            Some(id) => id.clone(),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                state.install_id = Some(id.clone());
                id
            }
        };
        state.last_cli_process_start_attempt = Some(now);
        (Some(id), true)
    })
    .ok()
    .flatten()
}

/// Confirm a reserved CLI `process_start` send was delivered, claiming the daily
/// slot. A no-op (writing nothing, so the file is never recreated) when the
/// install id no longer matches, i.e. an opt-out or `reset-id` landed during the
/// in-flight send. The attempt stamp was already written by
/// [`reserve_cli_process_start`]; this only stamps the confirmed-delivery slot.
pub fn confirm_cli_process_start(install_id: &str) {
    let result = update_state_locked(|state| {
        if state.install_id.as_deref() != Some(install_id) {
            return ((), false);
        }
        state.last_cli_process_start = Some(Utc::now());
        ((), true)
    });
    if let Err(e) = result {
        tracing::debug!(target: "telemetry", "failed to persist cli throttle stamp: {e}");
    }
}
