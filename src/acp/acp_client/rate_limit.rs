//! Rate-limit classification and the captured rejected-window reset time.

use crate::acp::state::RateLimitInfo;
use std::collections::HashMap;

/// Reset time comes only from a captured rejected-window epoch; the localized
/// message is displayed verbatim, never parsed.
pub(crate) fn classify_rate_limit_error(
    err: &agent_client_protocol::Error,
    captured_resets_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<RateLimitInfo> {
    let kind = err.data.as_ref()?.get("errorKind")?.as_str()?;
    (kind == "rate_limit").then(|| RateLimitInfo {
        status: err.message.clone(),
        resets_at: captured_resets_at,
        kind: kind.to_string(),
    })
}

/// Fallback for an outer error that kept only the serialized `errorKind`.
pub(crate) fn classify_rate_limit_from_message(
    message: &str,
    captured_resets_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<RateLimitInfo> {
    let matched = [
        "\"errorKind\":\"rate_limit\"",
        "\"errorKind\": \"rate_limit\"",
    ]
    .iter()
    .any(|fingerprint| message.contains(fingerprint));
    matched.then(|| RateLimitInfo {
        status: message.to_string(),
        resets_at: captured_resets_at,
        kind: "rate_limit".to_string(),
    })
}

/// A stored session ID the agent no longer knows, as opposed to an
/// unsupported session feature.
pub(crate) fn is_unsupported_session_error(err: &agent_client_protocol::Error) -> bool {
    const PHRASES: &[&str] = &[
        "unsupported acp session",
        "unknown session",
        "session not found",
        "no such session",
        "session does not exist",
    ];
    let msg = err.message.to_ascii_lowercase();
    PHRASES.iter().any(|phrase| msg.contains(phrase))
}

/// A rejected window from a `usage_update`'s `_meta["_claude/rateLimit"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RateLimitRejection {
    /// `rateLimitType`, or empty; keys captures so windows cannot overwrite
    /// each other.
    pub(super) window: String,
    pub(super) resets_at_secs: i64,
}

/// Only rejections are retained: a warning's epoch cannot be tied to the
/// window that later rejects (#3152).
pub(super) fn rate_limit_rejection_from_meta(
    meta: &Option<agent_client_protocol::schema::v1::Meta>,
) -> Option<RateLimitRejection> {
    let info = meta.as_ref()?.get("_claude/rateLimit")?;
    if info.get("status")?.as_str()? != "rejected" {
        return None;
    }
    let v = info.get("resetsAt")?;
    let raw = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    if raw <= 0 {
        return None;
    }
    // Seconds past ~year 5138 are really milliseconds.
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

/// The latest future reset, since every rejected window must clear.
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
    use serde_json::json;

    fn err(message: &str, data: Option<serde_json::Value>) -> agent_client_protocol::Error {
        let mut err = agent_client_protocol::Error::internal_error();
        err.message = message.into();
        err.data = data;
        err
    }

    #[test]
    fn classify_rate_limit_error_cases() {
        let limit = json!({ "errorKind": "rate_limit" });
        let captured = chrono::Utc::now() + chrono::Duration::minutes(10);

        // #3152: no captured epoch means an unknown reset, never a guess.
        let info =
            classify_rate_limit_error(&err("You've hit your limit", Some(limit.clone())), None)
                .unwrap();
        assert_eq!(info.kind, "rate_limit");
        assert!(info.status.contains("hit your limit"));
        assert_eq!(info.resets_at, None);

        let info = classify_rate_limit_error(&err("limit", Some(limit)), Some(captured)).unwrap();
        assert_eq!(info.resets_at, Some(captured));

        let with_reset = json!({ "errorKind": "rate_limit", "resets_at": "2099-01-01T00:00:00Z" });
        let info = classify_rate_limit_error(&err("x", Some(with_reset)), None).unwrap();
        assert_eq!(info.resets_at, None);

        let internal = json!({ "errorKind": "internal" });
        assert!(classify_rate_limit_error(&err("closed", Some(internal)), None).is_none());
        let invalid = agent_client_protocol::Error::invalid_params();
        assert!(classify_rate_limit_error(&invalid, None).is_none());

        // The same fingerprint inside a flattened error message.
        let captured = chrono::Utc::now() + chrono::Duration::minutes(20);
        for msg in [
            "ACP connection failed: Internal error: limit: {\n  \"errorKind\":\"rate_limit\"\n}",
            "{\n  \"errorKind\": \"rate_limit\"\n}",
        ] {
            let info = classify_rate_limit_from_message(msg, Some(captured)).unwrap();
            assert_eq!(info.kind, "rate_limit");
            assert_eq!(info.resets_at, Some(captured));
        }
        assert!(classify_rate_limit_from_message("connection refused", None).is_none());
    }

    #[test]
    fn rate_limit_reset_windows() {
        let secs = 4_102_444_800_i64;
        let meta = |value| {
            Some(serde_json::Map::from_iter([(
                "_claude/rateLimit".to_string(),
                value,
            )]))
        };
        let rejection = |window: &str| {
            Some(RateLimitRejection {
                window: window.to_string(),
                resets_at_secs: secs,
            })
        };
        let cases = [
            (
                meta(
                    json!({ "status": "rejected", "rateLimitType": "five_hour", "resetsAt": secs }),
                ),
                rejection("five_hour"),
            ),
            // #3028: millisecond epochs normalize to seconds.
            (
                meta(json!({ "status": "rejected", "resetsAt": secs * 1000 })),
                rejection(""),
            ),
            (meta(json!({ "status": "allowed", "resetsAt": secs })), None),
            (
                meta(json!({ "status": "allowed_warning", "resetsAt": secs })),
                None,
            ),
            (meta(json!({ "status": "rejected" })), None),
            (meta(json!({ "status": "rejected", "resetsAt": 0 })), None),
            (None, None),
            (
                Some(serde_json::Map::from_iter([(
                    "claudeCode".to_string(),
                    json!({}),
                )])),
                None,
            ),
        ];
        for (meta, want) in cases {
            assert_eq!(rate_limit_rejection_from_meta(&meta), want, "{meta:?}");
        }

        {
            let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
            let five_hour = now + chrono::Duration::hours(2);
            let seven_day = now + chrono::Duration::days(3);
            let past = now - chrono::Duration::minutes(1);
            let cases = [
                (vec![five_hour, seven_day], Some(seven_day)),
                (vec![five_hour, past], Some(five_hour)),
                (vec![], None),
                (vec![past], None),
            ];
            for (resets, want) in cases {
                let captures = std::sync::Mutex::new(
                    resets
                        .iter()
                        .enumerate()
                        .map(|(i, dt)| (i.to_string(), dt.timestamp()))
                        .collect(),
                );
                assert_eq!(captured_rate_limit_resets_at(&captures, now), want);
            }
        }
    }

    #[test]
    fn is_unsupported_session_error_matches_only_stale_session_rejections() {
        let cases = [
            ("Unsupported ACP session", true),
            ("Unknown session 01a040bb-...", true),
            ("session not found", true),
            ("no such session: abc", true),
            ("Session does not exist", true),
            ("You've hit your limit", false),
            ("transport closed", false),
            ("unsupported model", false),
            ("unknown tool", false),
            ("Unsupported content block in session/prompt", false),
            ("Method not found: session/prompt", false),
            ("Unsupported session mode", false),
            ("Unsupported session capability: fork", false),
        ];
        for (msg, expected) in cases {
            assert_eq!(
                is_unsupported_session_error(&err(msg, None)),
                expected,
                "{msg:?}"
            );
        }
    }
}
