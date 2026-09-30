//! Maps agent hook payloads to usage rows (see the spec's normalization table).

use serde_json::Value;

use super::UsageKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub kind: UsageKind,
    pub detail: Option<String>,
    pub agent_session_id: Option<String>,
}

pub fn normalize(agent: &str, payload: &Value) -> Option<Normalized> {
    let field = |key: &str| payload.get(key).and_then(Value::as_str);
    let event = field("hook_event_name")?;
    let (kind, detail) = if agent == "pi" {
        match event {
            "session_start" => (
                UsageKind::ContextStart,
                field("reason").map(|reason| if reason == "new" { "clear" } else { reason }),
            ),
            "session_shutdown" => (UsageKind::ContextEnd, field("reason")),
            "session_compact" => (
                UsageKind::Compact,
                Some(if field("reason") == Some("manual") {
                    "manual"
                } else {
                    "auto"
                }),
            ),
            "input" if matches!(field("source"), Some("interactive" | "rpc")) => {
                (UsageKind::Prompt, None)
            }
            "agent_settled" => (UsageKind::TurnEnd, None),
            _ => return None,
        }
    } else {
        match event {
            "SessionStart" => (UsageKind::ContextStart, field("source")),
            "SessionEnd" => (UsageKind::ContextEnd, field("reason")),
            "PostCompact" => (UsageKind::Compact, field("trigger")),
            "UserPromptSubmit" => (UsageKind::Prompt, None),
            "Stop" => (UsageKind::TurnEnd, None),
            "StopFailure" => (UsageKind::TurnEnd, Some("error")),
            _ => return None,
        }
    };
    Some(Normalized {
        kind,
        detail: detail.and_then(sanitize_detail),
        agent_session_id: field("session_id")
            .filter(|sid| crate::session::capture::is_valid_session_id(sid))
            .map(str::to_string),
    })
}

/// At most 32 chars of `[a-z0-9_]`; anything else is dropped rather than stored.
fn sanitize_detail(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payloads_map_to_kinds_and_details() {
        use UsageKind::*;
        const SID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let cases: Vec<(&str, Value, Option<(UsageKind, Option<&str>)>)> = vec![
            (
                "claude",
                json!({"hook_event_name":"SessionStart","source":"clear","session_id":SID}),
                Some((ContextStart, Some("clear"))),
            ),
            (
                "codex",
                json!({"hook_event_name":"SessionStart","source":"fork","session_id":SID}),
                Some((ContextStart, Some("fork"))),
            ),
            (
                "claude",
                json!({"hook_event_name":"SessionEnd","reason":"prompt_input_exit"}),
                Some((ContextEnd, Some("prompt_input_exit"))),
            ),
            (
                "claude",
                json!({"hook_event_name":"PostCompact","trigger":"auto"}),
                Some((Compact, Some("auto"))),
            ),
            (
                "codex",
                json!({"hook_event_name":"PostCompact","trigger":"manual"}),
                Some((Compact, Some("manual"))),
            ),
            (
                "claude",
                json!({"hook_event_name":"UserPromptSubmit","prompt":"hi"}),
                Some((Prompt, None)),
            ),
            (
                "claude",
                json!({"hook_event_name":"Stop"}),
                Some((TurnEnd, None)),
            ),
            (
                "claude",
                json!({"hook_event_name":"StopFailure"}),
                Some((TurnEnd, Some("error"))),
            ),
            ("claude", json!({"hook_event_name":"PreToolUse"}), None),
            ("claude", json!({"source":"clear"}), None),
            (
                "claude",
                json!({"hook_event_name":"SessionStart","source":"Weird Value!"}),
                Some((ContextStart, None)),
            ),
            (
                "claude",
                json!({"hook_event_name":"SessionStart","source":"x".repeat(33)}),
                Some((ContextStart, None)),
            ),
            (
                "pi",
                json!({"hook_event_name":"session_start","reason":"new"}),
                Some((ContextStart, Some("clear"))),
            ),
            (
                "pi",
                json!({"hook_event_name":"session_start","reason":"reload"}),
                Some((ContextStart, Some("reload"))),
            ),
            (
                "pi",
                json!({"hook_event_name":"session_shutdown","reason":"quit"}),
                Some((ContextEnd, Some("quit"))),
            ),
            (
                "pi",
                json!({"hook_event_name":"session_compact","reason":"manual"}),
                Some((Compact, Some("manual"))),
            ),
            (
                "pi",
                json!({"hook_event_name":"session_compact","reason":"threshold"}),
                Some((Compact, Some("auto"))),
            ),
            (
                "pi",
                json!({"hook_event_name":"session_compact","reason":"overflow"}),
                Some((Compact, Some("auto"))),
            ),
            (
                "pi",
                json!({"hook_event_name":"input","source":"interactive"}),
                Some((Prompt, None)),
            ),
            (
                "pi",
                json!({"hook_event_name":"input","source":"rpc"}),
                Some((Prompt, None)),
            ),
            (
                "pi",
                json!({"hook_event_name":"input","source":"extension"}),
                None,
            ),
            (
                "pi",
                json!({"hook_event_name":"agent_settled"}),
                Some((TurnEnd, None)),
            ),
            (
                "pi",
                json!({"hook_event_name":"SessionStart","source":"clear"}),
                None,
            ),
        ];
        for (agent, payload, expected) in cases {
            let got = normalize(agent, &payload).map(|n| (n.kind, n.detail));
            let expected = expected.map(|(k, d)| (k, d.map(str::to_string)));
            assert_eq!(got, expected, "{agent} {payload}");
        }
    }

    #[test]
    fn session_id_is_kept_only_when_safe() {
        let ok =
            json!({"hook_event_name":"Stop","session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"});
        let unsafe_id = json!({"hook_event_name":"Stop","session_id":"bad id;rm"});
        assert_eq!(
            normalize("claude", &ok)
                .unwrap()
                .agent_session_id
                .as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(
            normalize("claude", &unsafe_id).unwrap().agent_session_id,
            None
        );
    }
}
