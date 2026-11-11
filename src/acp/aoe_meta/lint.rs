//! `aoe acp lint-stream`: conformance check of a recorded agent-to-client
//! NDJSON stream, one JSON-RPC frame per line.

use super::ordering::{Arrival, ArrivalOrder};
use super::{Envelope, EnvelopeError, LeaseVerb, ProxyHello};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::BufRead;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// 1-based NDJSON line, 0 for a whole-stream finding.
    pub line: usize,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LintOptions {
    /// Fail a stream with no proxy hello.
    pub require_hello: bool,
}

/// A frame in the order the runtime would deliver it.
struct Seen {
    line: usize,
    update_kind: String,
    envelope: Option<Envelope>,
}

/// Replays the stream through the runtime's own ordering, so a repeated key,
/// an anchored frame and the hold bound behave exactly as they do live.
pub fn lint(reader: impl BufRead, opts: LintOptions) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut add = |line: usize, code: &'static str, message: String| {
        findings.push(Finding {
            line,
            code,
            message,
        })
    };
    let mut hello_line: Option<usize> = None;
    // The whole update, so a reused key on a different tool call is caught.
    let mut keys: HashMap<String, serde_json::Value> = HashMap::new();
    let mut leases: HashSet<String> = HashSet::new();
    let mut order: ArrivalOrder<Seen> = ArrivalOrder::default();
    let mut session: Option<String> = None;

    for (index, line) in reader.lines().enumerate() {
        let n = index + 1;
        let line = match line {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => line,
            Err(e) => {
                add(n, "io", e.to_string());
                break;
            }
        };
        let frame: serde_json::Value = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(e) => {
                add(n, "invalid_json", e.to_string());
                continue;
            }
        };

        // A session/new result with a new id starts a fresh key space, as the
        // runtime's reset commit does.
        if let Some(id) = frame.pointer("/result/sessionId").and_then(|v| v.as_str()) {
            if session.as_deref().is_some_and(|current| current != id) {
                keys.clear();
                order.session_reset();
            }
            session = Some(id.to_string());
        }

        if let Some(meta) = frame
            .pointer("/result/_meta")
            .and_then(|m| m.as_object())
            .filter(|m| m.get("aoe").is_some_and(|a| a.get("proxy").is_some()))
        {
            // Only the initialize result carries `protocolVersion`.
            if frame.pointer("/result/protocolVersion").is_none() {
                add(
                    n,
                    "hello_misplaced",
                    "proxy hello is only valid on the initialize response".into(),
                );
                continue;
            }
            match ProxyHello::from_meta(meta) {
                Ok(_) if hello_line.is_some() => {
                    add(n, "duplicate_hello", "second proxy hello".into())
                }
                Ok(_) => hello_line = Some(n),
                Err(e) => add(n, "invalid_hello", e.to_string()),
            }
            continue;
        }

        if frame.get("method").and_then(|m| m.as_str()) != Some("session/update") {
            continue;
        }
        let update = frame
            .pointer("/params/update")
            .unwrap_or(&serde_json::Value::Null);
        let update_kind = update
            .get("sessionUpdate")
            .and_then(|k| k.as_str())
            .unwrap_or_default()
            .to_string();
        // The runtime drops an update for another session before it reads the
        // envelope, but the frame still holds its adapter position.
        let foreign = frame
            .pointer("/params/sessionId")
            .and_then(|v| v.as_str())
            .is_some_and(|id| session.as_deref().is_some_and(|current| current != id));
        if foreign && super::is_anchored(update) {
            continue;
        }
        let envelope = match super::from_update_value(update) {
            _ if foreign => None,
            Ok(envelope) => envelope,
            Err(e) => {
                let code = match e {
                    EnvelopeError::UnsupportedVersion(_) => "unsupported_version",
                    EnvelopeError::EmptyKey => "empty_key",
                    EnvelopeError::BadLeaseTtl(_) => "bad_lease_ttl",
                    EnvelopeError::Malformed(_) => "bad_envelope",
                };
                add(n, code, e.to_string());
                // An invalid tailer frame still takes no adapter frame number.
                if super::is_anchored(update) {
                    continue;
                }
                None
            }
        };
        if let Some(env) = &envelope {
            match keys.get(&env.key) {
                Some(first) if first != update => add(
                    n,
                    "key_conflict",
                    format!(
                        "idempotency key {:?} reused with a different update",
                        env.key
                    ),
                ),
                Some(_) => {}
                None => {
                    keys.insert(env.key.clone(), update.clone());
                }
            }
        }
        let arrival = Arrival {
            key: envelope.as_ref().map(|e| e.key.as_str()),
            after_adapter_sequence: envelope.as_ref().and_then(|e| e.after_adapter_sequence),
        };
        let seen = Seen {
            line: n,
            update_kind,
            envelope: envelope.clone(),
        };
        let delivered = match order.arrive(arrival, seen) {
            Ok(delivered) => delivered,
            Err(e) => {
                add(n, "hold_overflow", e.to_string());
                continue;
            }
        };
        for seen in delivered {
            let Some(env) = seen.envelope else { continue };
            if seen.update_kind != "tool_call"
                && (env.parent_tool_call.is_some()
                    || env.memory_recall.is_some()
                    || env.plan_proposal.is_some())
            {
                add(
                    seen.line,
                    "primitive_on_wrong_update",
                    "parent_tool_call, memory_recall and plan_proposal are read on tool_call updates only"
                        .into(),
                );
            }
            if let Some(lease) = &env.lease {
                match lease.op {
                    LeaseVerb::Acquire => {
                        leases.insert(lease.id.clone());
                    }
                    LeaseVerb::Renew | LeaseVerb::Release if !leases.contains(&lease.id) => {
                        add(
                            seen.line,
                            "lease_unknown",
                            format!("lease {:?} was never acquired", lease.id),
                        );
                    }
                    LeaseVerb::Renew => {}
                    LeaseVerb::Release => {
                        leases.remove(&lease.id);
                    }
                }
            }
        }
    }

    for (anchor, seen) in order.drain_held() {
        add(
            seen.line,
            "sequence_beyond_stream",
            format!("after_adapter_sequence {anchor} is never reached by the stream"),
        );
    }
    if opts.require_hello && hello_line.is_none() {
        add(
            0,
            "hello_missing",
            "no _meta.aoe.proxy hello on an initialize response".into(),
        );
    }
    findings.sort_by_key(|f| f.line);
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"_meta":{"aoe":{"proxy":{"plugin_id":"p","protocol_version":1,"primitive_schema_version":1}}}}}"#;

    fn update_of(kind: &str, meta: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"{kind}","_meta":{{"aoe":{meta}}}}}}}}}"#
        )
    }

    fn tool_call(id: &str, meta: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"tool_call","toolCallId":"{id}","title":"T","_meta":{{"aoe":{meta}}}}}}}}}"#
        )
    }

    fn update(meta: &str) -> String {
        update_of("agent_message_chunk", meta)
    }

    fn in_session(id: &str, frame: String) -> String {
        frame.replace(r#""sessionId":"s""#, &format!(r#""sessionId":"{id}""#))
    }

    fn run(lines: &[String], require_hello: bool) -> Vec<&'static str> {
        let text = lines.join("\n");
        lint(text.as_bytes(), LintOptions { require_hello })
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    #[test]
    fn lint_cases() {
        let ok_lease = update(r#"{"v":1,"key":"a","lease":{"id":"l","kind":"busy","ttl_ms":100}}"#);
        // (name, stream, require_hello, expected finding codes)
        let cases: Vec<(&str, Vec<String>, bool, Vec<&str>)> = vec![
            ("clean", vec![HELLO.into(), ok_lease.clone()], true, vec![]),
            (
                "no hello required",
                vec![ok_lease.clone()],
                true,
                vec!["hello_missing"],
            ),
            ("no hello optional", vec![ok_lease.clone()], false, vec![]),
            (
                "bad hello",
                vec![HELLO.replace("\"protocol_version\":1", "\"protocol_version\":7")],
                false,
                vec!["invalid_hello"],
            ),
            (
                "duplicate hello",
                vec![HELLO.into(), HELLO.into()],
                false,
                vec!["duplicate_hello"],
            ),
            (
                "not json",
                vec!["{nope".into()],
                false,
                vec!["invalid_json"],
            ),
            (
                "missing key",
                vec![update(r#"{"v":1}"#)],
                false,
                vec!["bad_envelope"],
            ),
            (
                "empty key",
                vec![update(r#"{"v":1,"key":""}"#)],
                false,
                vec!["empty_key"],
            ),
            (
                "future version",
                vec![update(r#"{"v":2,"key":"a"}"#)],
                false,
                vec!["unsupported_version"],
            ),
            (
                "acquire without ttl",
                vec![update(
                    r#"{"v":1,"key":"a","lease":{"id":"l","kind":"busy"}}"#,
                )],
                false,
                vec!["bad_lease_ttl"],
            ),
            (
                "renew unknown lease",
                vec![update(
                    r#"{"v":1,"key":"a","lease":{"id":"l","kind":"task","op":"renew","ttl_ms":5}}"#,
                )],
                false,
                vec!["lease_unknown"],
            ),
            (
                "same key same payload",
                vec![
                    update(r#"{"v":1,"key":"a","ephemeral":true}"#),
                    update(r#"{"v":1,"key":"a","ephemeral":true}"#),
                ],
                false,
                vec![],
            ),
            (
                "same key other payload",
                vec![
                    update(r#"{"v":1,"key":"a","ephemeral":true}"#),
                    update(r#"{"v":1,"key":"a","ephemeral":false}"#),
                ],
                false,
                vec!["key_conflict"],
            ),
            (
                "key reused for a different update after a new session",
                vec![
                    tool_call("t1", r#"{"v":1,"key":"k"}"#),
                    r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.into(),
                    r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s2"}}"#.into(),
                    in_session("s2", tool_call("t2", r#"{"v":1,"key":"k"}"#)),
                ],
                false,
                vec![],
            ),
            (
                "a late old-session frame is not checked in the new key space",
                vec![
                    r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.into(),
                    in_session("s1", tool_call("t1", r#"{"v":1,"key":"k"}"#)),
                    r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s2"}}"#.into(),
                    in_session("s2", tool_call("t2", r#"{"v":1,"key":"k"}"#)),
                    in_session("s1", tool_call("t3", r#"{"v":1,"key":"k"}"#)),
                ],
                false,
                vec![],
            ),
            (
                "a late old-session frame still takes its adapter number",
                vec![
                    r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.into(),
                    r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s2"}}"#.into(),
                    in_session(
                        "s2",
                        update(r#"{"v":1,"key":"t","after_adapter_sequence":1,"ephemeral":true}"#),
                    ),
                    in_session("s1", update(r#"{"v":1,"key":"late"}"#)),
                ],
                false,
                vec![],
            ),
            (
                "key reused for a different update in the same session",
                vec![
                    r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.into(),
                    in_session("s1", tool_call("t1", r#"{"v":1,"key":"k"}"#)),
                    r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s1"}}"#.into(),
                    in_session("s1", tool_call("t2", r#"{"v":1,"key":"k"}"#)),
                ],
                false,
                vec!["key_conflict"],
            ),
            (
                "an invalid anchored frame is not an adapter frame",
                vec![
                    update(r#"{"v":1,"key":"","after_adapter_sequence":1}"#),
                    update(r#"{"v":1,"key":"t","after_adapter_sequence":1,"ephemeral":true}"#),
                ],
                false,
                vec!["empty_key", "sequence_beyond_stream"],
            ),
            (
                "same envelope on a different tool call",
                vec![
                    tool_call("t1", r#"{"v":1,"key":"k"}"#),
                    tool_call("t2", r#"{"v":1,"key":"k"}"#),
                ],
                false,
                vec!["key_conflict"],
            ),
            (
                "identical retransmission of a tool call",
                vec![
                    tool_call("t1", r#"{"v":1,"key":"k"}"#),
                    tool_call("t1", r#"{"v":1,"key":"k"}"#),
                ],
                false,
                vec![],
            ),
            (
                "anchor past the end",
                vec![
                    update(r#"{"v":1,"key":"a","after_adapter_sequence":2,"ephemeral":true}"#),
                    update(r#"{"v":1,"key":"b"}"#),
                ],
                false,
                vec!["sequence_beyond_stream"],
            ),
            (
                "a retransmitted adapter frame still takes its number",
                vec![
                    update(r#"{"v":1,"key":"k","ephemeral":true}"#),
                    update(r#"{"v":1,"key":"k","ephemeral":true}"#),
                    update(r#"{"v":1,"key":"t","after_adapter_sequence":2,"ephemeral":true}"#),
                ],
                false,
                vec![],
            ),
            (
                "anchor reached",
                vec![
                    update(r#"{"v":1,"key":"a","after_adapter_sequence":1,"ephemeral":true}"#),
                    update(r#"{"v":1,"key":"b"}"#),
                ],
                false,
                vec![],
            ),
        ];
        let hello_on_session_new = HELLO.replace("\"protocolVersion\":1,", "");
        let flood: Vec<String> = (0..=super::super::ordering::MAX_HELD)
            .map(|n| {
                update(&format!(
                    r#"{{"v":1,"key":"h{n}","after_adapter_sequence":2,"ephemeral":true}}"#
                ))
            })
            .chain([
                update(r#"{"v":1,"key":"p1"}"#),
                update(r#"{"v":1,"key":"p2"}"#),
            ])
            .collect();
        let mut cases = cases;
        cases.extend([
            (
                "hello on a session/new result",
                vec![hello_on_session_new],
                true,
                vec!["hello_missing", "hello_misplaced"],
            ),
            (
                "replayed release is dropped like at runtime",
                vec![
                    update(r#"{"v":1,"key":"a","lease":{"id":"l","kind":"busy","ttl_ms":5}}"#),
                    update(r#"{"v":1,"key":"r","lease":{"id":"l","kind":"busy","op":"release"}}"#),
                    update(r#"{"v":1,"key":"r","lease":{"id":"l","kind":"busy","op":"release"}}"#),
                ],
                false,
                vec![],
            ),
            (
                "anchored renewal that precedes its acquire on the wire",
                vec![
                    update(r#"{"v":1,"key":"r","after_adapter_sequence":2,"lease":{"id":"l","kind":"busy","op":"renew","ttl_ms":5}}"#),
                    update(r#"{"v":1,"key":"x"}"#),
                    update(r#"{"v":1,"key":"a","lease":{"id":"l","kind":"busy","ttl_ms":5}}"#),
                ],
                false,
                vec![],
            ),
            ("too many frames wait on an anchor", flood, false, vec!["hold_overflow"]),
            (
                "plan proposal off a tool call",
                vec![update(r#"{"v":1,"key":"a","plan_proposal":{"markdown":"- a"}}"#)],
                false,
                vec!["primitive_on_wrong_update"],
            ),
            (
                "plan proposal on a tool call",
                vec![update_of("tool_call", r#"{"v":1,"key":"a","plan_proposal":{"markdown":"- a"}}"#)],
                false,
                vec![],
            ),
        ]);
        for (name, stream, require_hello, expected) in cases {
            assert_eq!(run(&stream, require_hello), expected, "{name}");
        }
    }
}
