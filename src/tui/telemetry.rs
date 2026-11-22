//! The TUI reports telemetry to the Daemon, which owns consent and the install id and
//! applies the opt-in to everything it counts. Usage reports are fire-and-forget: a failed
//! post drops the report. A consent change that fails is kept for the App to show.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::telemetry::feature_use::FeatureUse;

static CONSENT_FAILURE: Mutex<Option<String>> = Mutex::new(None);
static CONSENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CONSENT_ORDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn post(path: &'static str, body: serde_json::Value) -> Result<(), String> {
    let endpoint = crate::acp::client::locate_daemon()
        .await
        .map_err(|e| e.to_string())?;
    crate::acp::client::HttpClient::new(endpoint)
        .map_err(|e| e.to_string())?
        .mutate(reqwest::Method::POST, path, Some(body))
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

fn report(path: &'static str, body: serde_json::Value) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        if let Err(error) = post(path, body).await {
            tracing::debug!(target: "telemetry", path, "telemetry report failed: {error}");
        }
    });
}

/// Saves an opt-in answer through the Daemon, which also marks the prompt answered. Only
/// the latest of several quick changes is sent; a failure is left for
/// [`take_consent_failure`].
pub(crate) fn set_consent(enabled: bool) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let sequence = CONSENT_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    runtime.spawn(async move {
        let _order = CONSENT_ORDER.lock().await;
        if CONSENT_SEQUENCE.load(Ordering::SeqCst) != sequence {
            return;
        }
        let body = serde_json::json!({ "enabled": enabled });
        if let Err(error) = post("/api/telemetry/consent", body).await {
            tracing::warn!(target: "telemetry", "telemetry consent not saved: {error}");
            *CONSENT_FAILURE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(format!("Telemetry setting not saved: {error}"));
        }
    });
}

/// The last consent change the Daemon did not save, once.
pub(crate) fn take_consent_failure() -> Option<String> {
    CONSENT_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

pub(crate) fn report_feature_use(feature: FeatureUse) {
    report(
        "/api/telemetry/feature-use",
        serde_json::json!({ "feature": feature.as_str() }),
    );
}

/// Marks the TUI as used in the Daemon's next snapshot.
pub(crate) fn report_seen() {
    report(
        "/api/telemetry/seen",
        serde_json::json!({ "surface": "tui" }),
    );
}
