//! Opt-in clean-only plugin auto-update sweep at startup.
//!
//! Gated on `updates.auto_update_plugins` (off by default). When on, the TUI and
//! `aoe serve` spawn [`spawn_if_enabled`] at startup; it checks installed
//! external plugins for updates and applies only the ones that need no new
//! consent. Anything that changes capabilities, build steps, or UI slots is
//! skipped and left for a manual `aoe plugin update`, so a background sweep never
//! grants new capabilities or runs a changed build step unattended, and never
//! deactivates a working plugin.
//!
//! ponytail: no cross-process lock around the sweep; it runs once at startup and
//! the pre-existing install/update path is itself unguarded. Add an on-disk
//! plugin-op lock if concurrent CLI/daemon mutation becomes a real problem.

use crate::session::Config;

use super::{install, update_check};

/// What a sweep did, for logging and tests.
#[derive(Debug, Default)]
pub struct SweepSummary {
    pub applied: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub errors: Vec<(String, String)>,
}

/// Check outdated external plugins and apply only the clean updates. Logs each
/// outcome. Safe to call regardless of the setting; callers gate on it via
/// [`spawn_if_enabled`].
pub async fn sweep() -> SweepSummary {
    let mut summary = SweepSummary::default();
    for status in update_check::outdated().await {
        if let Some(error) = &status.error {
            tracing::warn!(
                target: "plugin.auto_update",
                plugin = %status.id,
                %error,
                "could not check plugin for updates",
            );
            summary.errors.push((status.id.clone(), error.clone()));
            continue;
        }
        if !status.needs_update {
            continue;
        }
        match install::update_clean(&status.id).await {
            Ok(install::UpdateOutcome::Applied(report)) => {
                tracing::info!(
                    target: "plugin.auto_update",
                    plugin = %report.id,
                    version = %report.version,
                    "auto-updated plugin",
                );
                summary.applied.push(report.id);
            }
            Ok(install::UpdateOutcome::Skipped { id, reason }) => {
                tracing::info!(
                    target: "plugin.auto_update",
                    plugin = %id,
                    %reason,
                    "skipped plugin auto-update; run `aoe plugin update` to review",
                );
                summary.skipped.push((id, reason));
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(
                    target: "plugin.auto_update",
                    plugin = %status.id,
                    %error,
                    "plugin auto-update failed",
                );
                summary.errors.push((status.id, error));
            }
        }
    }
    summary
}

/// Spawn the sweep in the background when the setting opts in. Non-blocking so
/// startup is never delayed by network or git; the registry is reloaded inside
/// `install::update_clean` as each update lands.
pub fn spawn_if_enabled(config: &Config) {
    if !config.updates.auto_update_plugins {
        return;
    }
    tokio::spawn(async move {
        let summary = sweep().await;
        tracing::info!(
            target: "plugin.auto_update",
            applied = summary.applied.len(),
            skipped = summary.skipped.len(),
            errors = summary.errors.len(),
            "plugin auto-update sweep complete",
        );
    });
}
