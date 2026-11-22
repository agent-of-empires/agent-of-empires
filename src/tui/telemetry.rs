//! The TUI reports telemetry to the Daemon, which owns consent and the install id and
//! applies the opt-in to everything it counts. Reports are fire-and-forget: a failed post
//! drops the report and never delays the action that made it.

use crate::telemetry::feature_use::FeatureUse;

fn post(path: &'static str, body: serde_json::Value) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        let result = async {
            let endpoint = crate::acp::client::locate_daemon()
                .await
                .map_err(|e| e.to_string())?;
            crate::acp::client::HttpClient::new(endpoint)
                .map_err(|e| e.to_string())?
                .mutate(reqwest::Method::POST, path, Some(body))
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(target: "telemetry", path, "telemetry report failed: {error}");
        }
    });
}

/// Saves an opt-in answer through the Daemon, which also marks the prompt answered.
pub(crate) fn set_consent(enabled: bool) {
    post(
        "/api/telemetry/consent",
        serde_json::json!({ "enabled": enabled }),
    );
}

pub(crate) fn report_feature_use(feature: FeatureUse) {
    post(
        "/api/telemetry/feature-use",
        serde_json::json!({ "feature": feature.as_str() }),
    );
}

/// Marks the TUI as used in the Daemon's next snapshot.
pub(crate) fn report_seen() {
    post(
        "/api/telemetry/seen",
        serde_json::json!({ "surface": "tui" }),
    );
}
