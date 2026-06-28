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

use std::sync::Arc;

use crate::session::Config;

use super::{install, update_check};

/// Surfaces a consent-needed auto-update skip in-product. Kept abstract so this
/// module (which compiles in TUI-only builds) never references the serve-gated
/// plugin host. The `aoe serve` daemon implements it on `PluginHost`.
pub trait UpdateNotifier: Send + Sync {
    fn needs_approval(&self, plugin_id: &str, reason: &str);
}

#[cfg(feature = "serve")]
impl UpdateNotifier for super::host::PluginHost {
    fn needs_approval(&self, plugin_id: &str, reason: &str) {
        self.notify_host(
            plugin_id,
            super::ui_state::Tone::Warn,
            format!("Update for {plugin_id} needs approval"),
            Some(reason.to_string()),
        );
    }
}

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
///
/// When a `notifier` is present (the `aoe serve` daemon), an update skipped
/// because it needs fresh consent also surfaces a notification so the dashboard
/// shows it in-product instead of only logging a CLI instruction, unless the
/// user already dismissed that exact version.
pub async fn sweep(notifier: Option<&Arc<dyn UpdateNotifier>>) -> SweepSummary {
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
            Ok(install::UpdateOutcome::Skipped {
                id,
                reason,
                fingerprint,
            }) => {
                tracing::info!(
                    target: "plugin.auto_update",
                    plugin = %id,
                    %reason,
                    "skipped plugin auto-update; run `aoe plugin update` to review",
                );
                if let Some(notifier) = notifier {
                    if !already_dismissed(&id, &fingerprint) {
                        notifier.needs_approval(&id, &reason);
                    }
                }
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

/// Whether the user already dismissed in-app the exact version a sweep skipped,
/// so the sweep does not re-notify on every daemon restart.
fn already_dismissed(id: &str, fingerprint: &str) -> bool {
    Config::load()
        .ok()
        .and_then(|c| c.plugins.get(id).and_then(|p| p.dismissed_update.clone()))
        .as_deref()
        == Some(fingerprint)
}

/// Spawn the sweep in the background when the setting opts in. Non-blocking so
/// startup is never delayed by network or git; the registry is reloaded inside
/// `install::update_clean` as each update lands. `notifier` is the running
/// plugin host (`aoe serve`), used to surface consent-needed skips as
/// notifications; `None` in TUI-only contexts, where there is no ring.
pub fn spawn_if_enabled(config: &Config, notifier: Option<Arc<dyn UpdateNotifier>>) {
    if !config.updates.auto_update_plugins {
        return;
    }
    tokio::spawn(async move {
        let summary = sweep(notifier.as_ref()).await;
        tracing::info!(
            target: "plugin.auto_update",
            applied = summary.applied.len(),
            skipped = summary.skipped.len(),
            errors = summary.errors.len(),
            "plugin auto-update sweep complete",
        );
    });
}
