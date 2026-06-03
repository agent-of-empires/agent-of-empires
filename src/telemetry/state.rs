//! Persistence of the anonymous install id.
//!
//! Stored in a dedicated `<app_dir>/telemetry.json`, deliberately separate
//! from `config.toml`: users routinely paste their config into bug reports,
//! and the id leaking there would both expose it and corrupt distinct-install
//! counts. The file is created only on opt-in and deleted on opt-out.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::session::get_app_dir;

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
    let mut state = load_state();
    if let Some(id) = state.install_id.as_ref().filter(|s| !s.trim().is_empty()) {
        return Some(id.clone());
    }
    let id = uuid::Uuid::new_v4().to_string();
    state.install_id = Some(id.clone());
    if let Err(e) = save_state(&state) {
        tracing::debug!(target: "telemetry", "failed to persist install id: {e}");
        return None;
    }
    Some(id)
}

/// Delete the install id (and its file) on opt-out. Idempotent.
pub fn delete_install_id() -> Result<()> {
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

/// Whether a CLI `process_start` send is due. Due when the last *confirmed*
/// send is older than `success_gap` (or never) AND the last *attempt* is older
/// than `retry_gap` (or never). `success_gap` is the real once-per-day throttle
/// that bounds the only unbounded telemetry source; `retry_gap` bounds how often
/// a failed send is retried so a down endpoint can't turn every `aoe` invocation
/// into a fresh attempt. Caller is responsible for the opt-in gate. Read-only:
/// the stamps are written by [`record_cli_process_start`] after the send.
pub fn cli_process_start_due(success_gap: Duration, retry_gap: Duration) -> bool {
    let state = load_state();
    let now = Utc::now();
    // A stamp is "fresh" when its positive elapsed is still inside the gap. A
    // negative elapsed (clock skew) counts as not fresh, so the send is allowed.
    let fresh = |stamp: Option<DateTime<Utc>>, gap: Duration| match stamp {
        Some(last) => matches!((now - last).to_std(), Ok(elapsed) if elapsed < gap),
        None => false,
    };
    !fresh(state.last_cli_process_start, success_gap)
        && !fresh(state.last_cli_process_start_attempt, retry_gap)
}

/// Record a CLI `process_start` send. Always stamps the attempt (so `retry_gap`
/// bounds retries); stamps the confirmed-delivery slot only when `success`, so a
/// failed send leaves the daily slot open for the next invocation to retry.
pub fn record_cli_process_start(success: bool) {
    let mut state = load_state();
    let now = Utc::now();
    state.last_cli_process_start_attempt = Some(now);
    if success {
        state.last_cli_process_start = Some(now);
    }
    if let Err(e) = save_state(&state) {
        tracing::debug!(target: "telemetry", "failed to persist cli throttle stamp: {e}");
    }
}
