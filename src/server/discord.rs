//! Discord webhook notifications for AoE session status changes.
//!
//! This module is intentionally one-way. It does not connect to the Discord
//! Gateway, register commands, accept Discord input, or control sessions.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::push::StatusChange;
use super::AppState;
use crate::session::config::DiscordConfig;
use crate::session::Status;

const DISCORD_WEBHOOK_MAX_ATTEMPTS: usize = 4;
const DISCORD_RATE_LIMIT_FALLBACK_DELAY: Duration = Duration::from_millis(1_250);
const DISCORD_RATE_LIMIT_MAX_DELAY: Duration = Duration::from_secs(15);

pub fn spawn_consumer(state: Arc<AppState>, fallback: DiscordConfig) {
    let mut rx = state.status_tx.subscribe();
    let shutdown = state.shutdown.clone();
    crate::task_util::spawn_supervised(
        "server.discord.webhook",
        crate::task_util::PanicPolicy::Log,
        async move {
            info!(target: "server.discord", "Discord webhook worker starting");
            let client = reqwest::Client::new();
            loop {
                tokio::select! {
                    result = rx.recv() => {
                        match result {
                            Ok(change) => {
                                let config = latest_discord_config(&state, &fallback);
                                if let Err(e) = handle_status_change(&client, &config, change).await {
                                    warn!(target: "server.discord", "failed to send Discord webhook notification: {e}");
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!(target: "server.discord", skipped = n, "Discord webhook notification receiver lagged");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
            info!(target: "server.discord", "Discord webhook worker stopped");
        },
    );
}

fn latest_discord_config(state: &AppState, fallback: &DiscordConfig) -> DiscordConfig {
    crate::session::profile_config::resolve_config(&state.profile)
        .map(|config| config.discord)
        .unwrap_or_else(|e| {
            warn!(target: "server.discord", profile = %state.profile, "failed to reload Discord config: {e}");
            fallback.clone()
        })
}

async fn handle_status_change(
    client: &reqwest::Client,
    config: &DiscordConfig,
    change: StatusChange,
) -> anyhow::Result<()> {
    if !should_send_notification(config, change.new) {
        return Ok(());
    }
    let Some(webhook_url) = config
        .webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        debug!(target: "server.discord", "Discord webhook notification skipped: webhook_url is empty");
        return Ok(());
    };

    let payload = discord_webhook_payload(config, &change);
    send_webhook_json(client, webhook_url, payload).await
}

fn should_send_notification(config: &DiscordConfig, status: Status) -> bool {
    if !config.enabled {
        return false;
    }
    match status {
        Status::Waiting => config.notify_on_waiting,
        Status::Idle => config.notify_on_idle,
        Status::Error => config.notify_on_error,
        Status::Stopped => config.notify_on_stopped,
        _ => false,
    }
}

fn discord_webhook_payload(config: &DiscordConfig, change: &StatusChange) -> Value {
    let title = format!(
        "AoE {}: {}",
        status_summary(change.new),
        truncate_chars(&change.instance_title, 180)
    );
    let status_line = format!("{} -> {}", change.old.as_str(), change.new.as_str());
    let mut payload = json!({
        "embeds": [
            {
                "title": title,
                "description": format!("Status changed: `{status_line}`"),
                "color": status_color(change.new),
                "timestamp": change.at.to_rfc3339(),
                "fields": [
                    {
                        "name": "Session",
                        "value": truncate_chars(&change.instance_title, 1024),
                        "inline": true
                    },
                    {
                        "name": "Status",
                        "value": status_line,
                        "inline": true
                    },
                    {
                        "name": "Session ID",
                        "value": format!("`{}`", truncate_chars(&change.instance_id, 120)),
                        "inline": false
                    }
                ]
            }
        ]
    });

    if let Some(username) = config
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload["username"] = json!(truncate_chars(username, 80));
    }
    if let Some(mention) = config
        .mention
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload["content"] = json!(truncate_chars(mention, 1900));
    }

    payload
}

fn status_summary(status: Status) -> &'static str {
    match status {
        Status::Waiting => "needs input",
        Status::Idle => "finished",
        Status::Error => "errored",
        Status::Stopped => "stopped",
        Status::Running => "running",
        Status::Creating => "creating",
        Status::Starting => "starting",
        Status::Deleting => "deleting",
        Status::Unknown => "updated",
    }
}

fn status_color(status: Status) -> u32 {
    match status {
        Status::Waiting => 0xf2c94c,
        Status::Idle => 0x27ae60,
        Status::Error => 0xeb5757,
        Status::Stopped => 0x828282,
        Status::Running => 0x2f80ed,
        _ => 0x9b51e0,
    }
}

async fn send_webhook_json(
    client: &reqwest::Client,
    webhook_url: &str,
    payload: Value,
) -> anyhow::Result<()> {
    for attempt in 1..=DISCORD_WEBHOOK_MAX_ATTEMPTS {
        let response = client.post(webhook_url).json(&payload).send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::TOO_MANY_REQUESTS && attempt < DISCORD_WEBHOOK_MAX_ATTEMPTS {
            let delay = discord_rate_limit_delay(&headers, &body)
                .unwrap_or(DISCORD_RATE_LIMIT_FALLBACK_DELAY);
            warn!(
                target: "server.discord",
                attempt,
                delay_ms = delay.as_millis(),
                "Discord webhook rate limited; retrying"
            );
            tokio::time::sleep(delay).await;
            continue;
        }

        anyhow::bail!(
            "Discord webhook returned {status}: {}",
            truncate_chars(body.trim(), 500)
        );
    }

    anyhow::bail!("Discord webhook retry attempts exhausted")
}

fn discord_rate_limit_delay(headers: &HeaderMap, body: &str) -> Option<Duration> {
    let body_delay = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("retry_after").and_then(Value::as_f64))
        .and_then(duration_from_retry_after_seconds);
    if body_delay.is_some() {
        return body_delay;
    }

    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(duration_from_retry_after_seconds)
}

fn duration_from_retry_after_seconds(seconds: f64) -> Option<Duration> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds).min(DISCORD_RATE_LIMIT_MAX_DELAY))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use reqwest::header::HeaderValue;

    fn config() -> DiscordConfig {
        DiscordConfig {
            enabled: true,
            webhook_url: Some("https://discord.com/api/webhooks/1/token".to_string()),
            username: Some("AoE".to_string()),
            mention: Some("<@123>".to_string()),
            notify_on_waiting: true,
            notify_on_idle: true,
            notify_on_error: true,
            notify_on_stopped: false,
        }
    }

    fn change(new: Status) -> StatusChange {
        StatusChange {
            instance_id: "abc123".to_string(),
            instance_title: "vetudy - Saracens".to_string(),
            old: Status::Running,
            new,
            at: Utc::now(),
        }
    }

    #[test]
    fn webhook_notifications_follow_status_toggles() {
        let mut config = config();

        assert!(should_send_notification(&config, Status::Waiting));
        assert!(should_send_notification(&config, Status::Idle));
        assert!(should_send_notification(&config, Status::Error));
        assert!(!should_send_notification(&config, Status::Stopped));
        assert!(!should_send_notification(&config, Status::Running));

        config.enabled = false;
        assert!(!should_send_notification(&config, Status::Idle));
    }

    #[test]
    fn webhook_payload_contains_session_status_and_optional_metadata() {
        let payload = discord_webhook_payload(&config(), &change(Status::Idle));

        assert_eq!(payload["username"], "AoE");
        assert_eq!(payload["content"], "<@123>");
        assert_eq!(
            payload["embeds"][0]["title"],
            "AoE finished: vetudy - Saracens"
        );
        assert_eq!(
            payload["embeds"][0]["description"],
            "Status changed: `running -> idle`"
        );
        assert_eq!(payload["embeds"][0]["fields"][2]["value"], "`abc123`");
    }

    #[test]
    fn discord_rate_limit_delay_prefers_json_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("9"));

        assert_eq!(
            discord_rate_limit_delay(&headers, r#"{"retry_after":0.75}"#),
            Some(Duration::from_millis(750))
        );
    }

    #[test]
    fn discord_rate_limit_delay_uses_retry_after_header() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("2"));

        assert_eq!(
            discord_rate_limit_delay(&headers, "{}"),
            Some(Duration::from_secs(2))
        );
    }
}
