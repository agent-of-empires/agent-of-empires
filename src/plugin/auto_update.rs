//! Opt-in clean-only plugin auto-update sweep at startup.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::session::Config;

use super::{install, update_check};

pub trait UpdateNotifier: Send + Sync {
    fn needs_approval(&self, plugin_id: &str, reason: &str);
    fn update_applied(
        self: Arc<Self>,
        plugin_id: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

impl UpdateNotifier for super::host::PluginHost {
    fn needs_approval(&self, plugin_id: &str, reason: &str) {
        self.notify_host(
            plugin_id,
            super::ui_state::Tone::Warn,
            format!("Update for {plugin_id} needs approval"),
            Some(reason.to_string()),
        );
    }

    fn update_applied(
        self: Arc<Self>,
        plugin_id: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            self.restart_worker(&plugin_id, &super::registry()).await;
        })
    }
}

#[derive(Debug, Default)]
pub struct SweepSummary {
    pub applied: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub errors: Vec<(String, String)>,
}

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
                match notifier {
                    Some(notifier) => Arc::clone(notifier).update_applied(report.id.clone()).await,
                    None => {
                        if let install::LiveRestart::DaemonStale { reason } =
                            install::restart_worker_live(&report.id).await
                        {
                            tracing::warn!(
                                target: "plugin.auto_update",
                                plugin = %report.id,
                                %reason,
                                "daemon did not reload the updated plugin; its worker runs the old build",
                            );
                        }
                    }
                }
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

fn already_dismissed(id: &str, fingerprint: &str) -> bool {
    Config::load()
        .ok()
        .and_then(|c| c.plugins.get(id).and_then(|p| p.dismissed_update.clone()))
        .as_deref()
        == Some(fingerprint)
}

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
