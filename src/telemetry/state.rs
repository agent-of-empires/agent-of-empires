//! Install id persistence in `<app_dir>/telemetry.json`, kept out of `config.toml` so it is
//! not pasted into bug reports. Created on opt-in, deleted on opt-out.

use anyhow::Result;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::get_app_dir;

/// Never removed, so two processes cannot race to recreate and re-lock it.
const STATE_LOCK_FILENAME: &str = ".telemetry.lock";

/// Telemetry must never stall a command; a contended lock skips the update.
const LOCK_ACQUIRE_BUDGET: Duration = Duration::from_millis(500);

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Always taken before the flock so lock order cannot invert.
static STATE_RMW_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize, Deserialize)]
struct TelemetryState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
    /// Stamped only on a confirmed send. The alias keeps the pre-rename field's stamp.
    #[serde(
        default,
        alias = "last_cli_process_start",
        skip_serializing_if = "Option::is_none"
    )]
    last_cli_usage_flush: Option<DateTime<Utc>>,
    #[serde(
        default,
        alias = "last_cli_process_start_attempt",
        skip_serializing_if = "Option::is_none"
    )]
    last_cli_usage_flush_attempt: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    cli_command_counts: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli_usage_window_start: Option<DateTime<Utc>>,
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
    // The id is the distinct-install key; keep it owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

struct StateFlock {
    file: std::fs::File,
}

impl Drop for StateFlock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

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

/// Blocking, for explicit opt-out and reset: the file must actually be deleted.
fn acquire_state_flock_blocking() -> Result<StateFlock> {
    let file = open_state_lock_file()?;
    FileExt::lock_exclusive(&file)?;
    Ok(StateFlock { file })
}

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

/// `f` returns `(value, dirty)`; state is saved only when dirty. `Err` when the lock is
/// contended past the budget.
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

pub fn install_id() -> Option<String> {
    load_state().install_id.filter(|s| !s.trim().is_empty())
}

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

pub fn delete_install_id() -> Result<()> {
    let _guard = STATE_RMW_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _flock = acquire_state_flock_blocking()?;
    let path = state_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn reset_install_id() -> Option<String> {
    if let Err(e) = delete_install_id() {
        tracing::debug!(target: "telemetry", "failed to delete install id during reset: {e}");
    }
    ensure_install_id()
}

pub fn record_cli_command(name: &str) {
    // Never persist a name outside the clap vocabulary; wire filtering does not protect the file.
    if !crate::cli::CLI_COMMAND_NAMES.contains(&name) {
        return;
    }
    let result = update_state_locked(|state| {
        *state
            .cli_command_counts
            .entry(name.to_string())
            .or_insert(0) += 1;
        if state.cli_usage_window_start.is_none() {
            state.cli_usage_window_start = Some(Utc::now());
        }
        ((), true)
    });
    if let Err(e) = result {
        tracing::debug!(target: "telemetry", "failed to persist cli command count: {e}");
    }
}

pub fn cli_usage_window() -> (BTreeMap<String, u32>, Option<DateTime<Utc>>) {
    let state = load_state();
    (state.cli_command_counts, state.cli_usage_window_start)
}

pub fn cli_usage_due(success_gap: Duration, retry_gap: Duration) -> bool {
    let state = load_state();
    let now = Utc::now();
    // Negative elapsed (clock skew) counts as not fresh.
    let fresh = |stamp: Option<DateTime<Utc>>, gap: Duration| match stamp {
        Some(last) => matches!((now - last).to_std(), Ok(elapsed) if elapsed < gap),
        None => false,
    };
    !fresh(state.last_cli_usage_flush, success_gap)
        && !fresh(state.last_cli_usage_flush_attempt, retry_gap)
}

/// A failed send keeps the counts and the daily slot for retry.
pub fn record_cli_usage_flush(success: bool) {
    let result = update_state_locked(|state| {
        let now = Utc::now();
        state.last_cli_usage_flush_attempt = Some(now);
        if success {
            state.last_cli_usage_flush = Some(now);
            state.cli_command_counts.clear();
            state.cli_usage_window_start = None;
        }
        ((), true)
    });
    if let Err(e) = result {
        tracing::debug!(target: "telemetry", "failed to persist cli usage flush state: {e}");
    }
}
