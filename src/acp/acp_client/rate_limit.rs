//! Rate-limit classification, and the rejected-window reset time aoe
//! reports from captured usage metadata.

use crate::acp::state::RateLimitInfo;
use std::collections::HashMap;

/// Classify a structured prompt error as a rate limit. Reset time comes only
/// from a separately captured rejected-window epoch; localized message text is
/// displayed verbatim but never parsed or guessed.
pub(crate) fn classify_rate_limit_error(
    err: &agent_client_protocol::Error,
    captured_resets_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<RateLimitInfo> {
    let data = err.data.as_ref()?;
    let kind = data.get("errorKind").and_then(|v| v.as_str())?;
    if kind != "rate_limit" {
        return None;
    }
    Some(RateLimitInfo {
        status: err.message.clone(),
        resets_at: captured_resets_at,
        kind: kind.to_string(),
    })
}

/// Fallback for an outer error that preserved only the adapter's serialized
/// `errorKind` fingerprint.
pub(crate) fn classify_rate_limit_from_message(
    message: &str,
    captured_resets_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<RateLimitInfo> {
    if !message.contains("\"errorKind\":\"rate_limit\"")
        && !message.contains("\"errorKind\": \"rate_limit\"")
    {
        return None;
    }
    Some(RateLimitInfo {
        status: message.to_string(),
        resets_at: captured_resets_at,
        kind: "rate_limit".to_string(),
    })
}

/// One observation of the SDK's rate-limit state, as forwarded by
/// claude-agent-acp under a `usage_update`'s
/// `_meta["_claude/rateLimit"]` (an `SDKRateLimitInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RateLimitRejection {
    /// `rateLimitType` ("five_hour", "seven_day", ...), or empty when the
    /// adapter omitted it. Used as the per-window map key so one window's
    /// reset cannot overwrite another's.
    pub(super) window: String,
    /// `resetsAt`, unix SECONDS.
    pub(super) resets_at_secs: i64,
}

/// Extract only rejected rate-limit windows. Warning epochs cannot be matched
/// reliably to a later rejected window. Millisecond epochs are normalized.
pub(super) fn rate_limit_rejection_from_meta(
    meta: &Option<agent_client_protocol::schema::v1::Meta>,
) -> Option<RateLimitRejection> {
    let info = meta.as_ref()?.get("_claude/rateLimit")?;
    if info.get("status").and_then(|v| v.as_str())? != "rejected" {
        return None;
    }
    let v = info.get("resetsAt")?;
    let raw = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    if raw <= 0 {
        return None;
    }
    // Anthropic reports unix seconds; anything past ~year 5138 in seconds
    // is really milliseconds.
    let resets_at_secs = if raw > 100_000_000_000 {
        raw / 1000
    } else {
        raw
    };
    Some(RateLimitRejection {
        window: info
            .get("rateLimitType")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        resets_at_secs,
    })
}

/// Return the latest future reset because every rejected window must clear.
/// Expired observations are ignored and missing data remains unknown.
pub(super) fn captured_rate_limit_resets_at(
    captures: &std::sync::Mutex<HashMap<String, i64>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    captures
        .lock()
        .expect("rate-limit capture mutex poisoned")
        .values()
        .filter_map(|secs| chrono::DateTime::from_timestamp(*secs, 0))
        .filter(|dt| *dt > now)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3152: with no captured rejection epoch the reset is unknown, and an
    // unknown reset must stay unknown. The old `now + 1h` fallback showed a
    // fabricated clock time in the banner.
    #[test]
    fn classify_rate_limit_recognises_data_errorkind_without_inventing_a_reset() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "You've hit your limit · resets 12:10pm (Europe/Paris)".into();
        err.data = Some(serde_json::json!({ "errorKind": "rate_limit" }));
        let info = classify_rate_limit_error(&err, None).expect("classified");
        assert_eq!(info.kind, "rate_limit");
        assert!(info.status.contains("hit your limit"));
        assert_eq!(info.resets_at, None);
    }

    // The captured rejection epoch is the only source of a reset time. #3028.
    #[test]
    fn classify_rate_limit_uses_captured_resets_at() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "You've hit your limit".into();
        err.data = Some(serde_json::json!({ "errorKind": "rate_limit" }));
        let captured = chrono::Utc::now() + chrono::Duration::minutes(10);
        let info = classify_rate_limit_error(&err, Some(captured)).expect("classified");
        assert_eq!(info.resets_at, Some(captured));
    }

    // The real adapter's error data is `{ errorKind }` only, so a reset in
    // there is not parsed; the captured epoch decides. #3152.
    #[test]
    fn classify_rate_limit_ignores_reset_in_error_data() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "rate limited".into();
        err.data = Some(serde_json::json!({
            "errorKind": "rate_limit",
            "resets_at": "2099-01-01T00:00:00Z",
        }));
        assert_eq!(
            classify_rate_limit_error(&err, None)
                .expect("classified")
                .resets_at,
            None
        );
    }

    #[test]
    fn classify_rate_limit_ignores_unrelated_errors() {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = "transport closed".into();
        err.data = Some(serde_json::json!({ "errorKind": "internal" }));
        assert!(classify_rate_limit_error(&err, None).is_none());

        let err = agent_client_protocol::Error::invalid_params();
        assert!(classify_rate_limit_error(&err, None).is_none());
    }

    #[test]
    fn classify_rate_limit_from_message_matches_acp_fingerprint() {
        let msg = "ACP connection failed: Internal error: You've hit your limit · resets 12:10pm (Europe/Paris): {\n  \"errorKind\":\"rate_limit\"\n}";
        let info = classify_rate_limit_from_message(msg, None).expect("classified");
        assert_eq!(info.kind, "rate_limit");
        // Spaced variant the adapter sometimes emits.
        let info_spaced =
            classify_rate_limit_from_message("{\n  \"errorKind\": \"rate_limit\"\n}", None)
                .expect("classified");
        assert_eq!(info_spaced.kind, "rate_limit");
        assert!(classify_rate_limit_from_message("connection refused", None).is_none());
    }

    #[test]
    fn classify_rate_limit_from_message_uses_captured_resets_at() {
        let msg = "{\n  \"errorKind\":\"rate_limit\"\n}";
        let captured = chrono::Utc::now() + chrono::Duration::minutes(20);
        let info = classify_rate_limit_from_message(msg, Some(captured)).expect("classified");
        assert_eq!(info.resets_at, Some(captured));
    }

    fn rate_limit_meta(
        value: serde_json::Value,
    ) -> Option<agent_client_protocol::schema::v1::Meta> {
        let mut meta = serde_json::Map::new();
        meta.insert("_claude/rateLimit".to_string(), value);
        Some(meta)
    }

    // #3028: resetsAt from the usage_update `_meta` is unix seconds; a
    // millisecond-scale value must be normalized so it can't resolve to a
    // far-future year.
    #[test]
    fn rate_limit_rejection_from_meta_reads_window_and_guards_units() {
        let secs = 4_102_444_800_i64; // 2100-01-01 in seconds
        assert_eq!(
            rate_limit_rejection_from_meta(&rate_limit_meta(serde_json::json!({
                "status": "rejected",
                "rateLimitType": "five_hour",
                "resetsAt": secs,
            }))),
            Some(RateLimitRejection {
                window: "five_hour".to_string(),
                resets_at_secs: secs,
            })
        );

        // Millisecond-scale value normalizes back to seconds; a missing
        // window keys under the empty string.
        assert_eq!(
            rate_limit_rejection_from_meta(&rate_limit_meta(serde_json::json!({
                "status": "rejected",
                "resetsAt": secs * 1000,
            }))),
            Some(RateLimitRejection {
                window: String::new(),
                resets_at_secs: secs,
            })
        );

        // No meta, or meta without the rate-limit key, yields nothing.
        assert_eq!(rate_limit_rejection_from_meta(&None), None);
        let mut other = serde_json::Map::new();
        other.insert("claudeCode".to_string(), serde_json::json!({}));
        assert_eq!(rate_limit_rejection_from_meta(&Some(other)), None);
    }

    // #3152: a warning carries a real epoch, but nothing ties it to the
    // window that later rejects, so it must not be retained. Retaining it is
    // what let a seven-day warning answer a five-hour rejection.
    #[test]
    fn rate_limit_rejection_from_meta_ignores_non_rejections() {
        let secs = 4_102_444_800_i64;
        for status in ["allowed", "allowed_warning"] {
            assert_eq!(
                rate_limit_rejection_from_meta(&rate_limit_meta(serde_json::json!({
                    "status": status,
                    "rateLimitType": "seven_day",
                    "resetsAt": secs,
                }))),
                None,
                "status {status} must not be retained"
            );
        }
        // A rejection without a usable epoch is nothing to retain either.
        assert_eq!(
            rate_limit_rejection_from_meta(&rate_limit_meta(
                serde_json::json!({ "status": "rejected" })
            )),
            None
        );
        assert_eq!(
            rate_limit_rejection_from_meta(&rate_limit_meta(serde_json::json!({
                "status": "rejected",
                "resetsAt": 0,
            }))),
            None
        );
    }

    // #3152: every window that rejected has to clear before the session can
    // run again, so the last reset still ahead of `now` is the answer.
    // Windows that already rolled over are ignored.
    #[test]
    fn captured_rate_limit_resets_at_takes_the_last_future_window() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("now");
        let five_hour = now + chrono::Duration::hours(2);
        let seven_day = now + chrono::Duration::days(3);
        let captures = std::sync::Mutex::new(HashMap::from([
            ("five_hour".to_string(), five_hour.timestamp()),
            ("seven_day".to_string(), seven_day.timestamp()),
        ]));
        assert_eq!(
            captured_rate_limit_resets_at(&captures, now),
            Some(seven_day)
        );

        // The seven-day window rolled over; only the five-hour one is live.
        let stale = std::sync::Mutex::new(HashMap::from([
            ("five_hour".to_string(), five_hour.timestamp()),
            (
                "seven_day".to_string(),
                (now - chrono::Duration::days(1)).timestamp(),
            ),
        ]));
        assert_eq!(captured_rate_limit_resets_at(&stale, now), Some(five_hour));

        // Nothing captured, or everything already past, is an unknown reset.
        assert_eq!(
            captured_rate_limit_resets_at(&std::sync::Mutex::new(HashMap::new()), now),
            None
        );
        let all_past = std::sync::Mutex::new(HashMap::from([(
            "five_hour".to_string(),
            (now - chrono::Duration::minutes(1)).timestamp(),
        )]));
        assert_eq!(captured_rate_limit_resets_at(&all_past, now), None);
    }
}
