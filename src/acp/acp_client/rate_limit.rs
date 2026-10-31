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
        status: super::errors::public_error_message(err),
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
    // 0.99.0+ answers an unknown id with `-32002` and `data.sessionId`.
    if err.code == agent_client_protocol::ErrorCode::ResourceNotFound
        && err
            .data
            .as_ref()
            .is_some_and(|d| d.get("sessionId").is_some())
    {
        return true;
    }
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

/// Extra usage keeps serving turns once the subscription window rejects.
fn overage_covers(info: &serde_json::Value) -> bool {
    matches!(
        info.get("overageStatus").and_then(|s| s.as_str()),
        Some("allowed" | "allowed_warning")
    )
}

/// Only rejections are retained: a warning's epoch cannot be tied to the
/// window that later rejects (#3152).
pub(super) fn rate_limit_rejection_from_meta(
    meta: &Option<agent_client_protocol::schema::v1::Meta>,
) -> Option<RateLimitRejection> {
    let info = meta.as_ref()?.get("_claude/rateLimit")?;
    if info.get("status")?.as_str()? != "rejected" || overage_covers(info) {
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

/// Records a rejected window and drops one a later report shows has recovered,
/// so a stale rejection cannot park a turn. Returns whether a rejection was kept.
pub(super) fn record_rate_limit_report(
    captures: &std::sync::Mutex<HashMap<String, i64>>,
    meta: &Option<agent_client_protocol::schema::v1::Meta>,
) -> bool {
    let Some(info) = meta.as_ref().and_then(|m| m.get("_claude/rateLimit")) else {
        return false;
    };
    let mut captures = captures.lock().expect("rate-limit capture mutex poisoned");
    if let Some(r) = rate_limit_rejection_from_meta(meta) {
        captures.insert(r.window, r.resets_at_secs);
        return true;
    }
    if overage_covers(info)
        || info
            .get("status")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s != "rejected")
    {
        let window = info.get("rateLimitType").and_then(|v| v.as_str());
        captures.remove(window.unwrap_or_default());
    }
    false
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

/// A turn nobody prompted ends without an error, so a still-future rejected
/// window is the only evidence it ended on the limit. Drains the captures under
/// one lock so a window recorded meanwhile is never cleared unobserved.
pub(super) fn take_rate_limit_from_captured_rejection(
    captures: &std::sync::Mutex<HashMap<String, i64>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<RateLimitInfo> {
    let drained = std::mem::take(&mut *captures.lock().expect("rate-limit capture mutex poisoned"));
    drained
        .values()
        .filter_map(|secs| chrono::DateTime::from_timestamp(*secs, 0))
        .filter(|dt| *dt > now)
        .max()
        .map(|resets_at| RateLimitInfo {
            status: "usage limit reached".to_string(),
            resets_at: Some(resets_at),
            kind: "rate_limit".to_string(),
        })
}

/// Captures belong to the turn that observed them, so each turn boundary drops them.
pub(super) fn clear_captured_rejections(captures: &std::sync::Mutex<HashMap<String, i64>>) {
    captures
        .lock()
        .expect("rate-limit capture mutex poisoned")
        .clear();
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
            (
                meta(
                    json!({ "status": "rejected", "resetsAt": secs, "overageStatus": "rejected" }),
                ),
                rejection(""),
            ),
            (
                meta(json!({ "status": "rejected", "resetsAt": secs, "overageStatus": "allowed" })),
                None,
            ),
            (
                meta(
                    json!({ "status": "rejected", "resetsAt": secs, "overageStatus": "allowed_warning" }),
                ),
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

        // A window that recovers, or that extra usage starts covering, after
        // rejecting is dropped; others stay.
        let report = |status: &str, overage: &str, window: &str| {
            meta(json!({
                "status": status,
                "overageStatus": overage,
                "rateLimitType": window,
                "resetsAt": secs,
            }))
        };
        let captures = std::sync::Mutex::new(HashMap::new());
        for (status, overage, window, kept, want) in [
            ("rejected", "rejected", "five_hour", true, vec!["five_hour"]),
            (
                "rejected",
                "rejected",
                "seven_day",
                true,
                vec!["five_hour", "seven_day"],
            ),
            ("allowed", "rejected", "five_hour", false, vec!["seven_day"]),
            (
                "allowed_warning",
                "rejected",
                "other",
                false,
                vec!["seven_day"],
            ),
            ("rejected", "allowed", "seven_day", false, vec![]),
        ] {
            assert_eq!(
                record_rate_limit_report(&captures, &report(status, overage, window)),
                kept
            );
            let mut have: Vec<_> = captures.lock().unwrap().keys().cloned().collect();
            have.sort();
            assert_eq!(have, want, "{status} {overage} {window}");
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
                let info = take_rate_limit_from_captured_rejection(&captures, now);
                assert_eq!(info.as_ref().and_then(|i| i.resets_at), want);
                assert!(info.is_none_or(|i| i.kind == "rate_limit"));
                assert!(take_rate_limit_from_captured_rejection(&captures, now).is_none());
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

        // 0.99.0+ rejects an unknown id with a bare `-32002` and no phrase.
        let not_found = |data| {
            let mut e = err("Resource not found", data);
            e.code = agent_client_protocol::ErrorCode::ResourceNotFound;
            e
        };
        assert!(is_unsupported_session_error(&not_found(Some(
            json!({ "sessionId": "abc" })
        ))));
        assert!(!is_unsupported_session_error(&not_found(None)));
        assert!(!is_unsupported_session_error(&not_found(Some(
            json!({ "uri": "file:///x" })
        ))));
    }
}
