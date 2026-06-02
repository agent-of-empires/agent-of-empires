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
    /// Last time a CLI `process_start` was emitted, used to throttle the one
    /// unbounded event source to at most once per install per day. Long-lived
    /// surfaces (TUI / serve) emit once per launch and need no throttle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_cli_process_start: Option<DateTime<Utc>>,
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

/// Claim the once-per-`min_gap` CLI `process_start` slot. Returns `true` and
/// stamps "now" when the last emission is older than `min_gap` (or never),
/// `false` otherwise. This bounds the only unbounded telemetry source: a user
/// scripting `aoe` in a loop emits at most one CLI `process_start` per window
/// instead of one per invocation. Caller is responsible for the opt-in gate.
pub fn claim_cli_process_start_slot(min_gap: Duration) -> bool {
    let mut state = load_state();
    let now = Utc::now();
    if let Some(last) = state.last_cli_process_start {
        // A positive elapsed shorter than the window means "too soon". A
        // negative elapsed (clock skew) falls through and is allowed.
        if let Ok(elapsed) = (now - last).to_std() {
            if elapsed < min_gap {
                return false;
            }
        }
    }
    state.last_cli_process_start = Some(now);
    if let Err(e) = save_state(&state) {
        // Persisting the stamp failed; allow this send rather than dropping it,
        // and try again to persist next time.
        tracing::debug!(target: "telemetry", "failed to persist cli throttle stamp: {e}");
    }
    true
}
