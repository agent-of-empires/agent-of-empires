//! `aoe acp lint-stream`: conformance check of a recorded agent-to-client
//! NDJSON stream, one JSON-RPC frame per line.

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
    let mut adapter_frames: u64 = 0;
    let mut keys: HashMap<String, Envelope> = HashMap::new();
    let mut leases: HashSet<String> = HashSet::new();
    let mut anchors: Vec<(usize, u64)> = Vec::new();

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

        if let Some(meta) = frame
            .pointer("/result/_meta")
            .and_then(|m| m.as_object())
            .filter(|m| m.get("aoe").is_some_and(|a| a.get("proxy").is_some()))
        {
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
        let env = match super::from_update_value(update) {
            Ok(env) => env,
            Err(e) => {
                let code = match e {
                    EnvelopeError::UnsupportedVersion(_) => "unsupported_version",
                    EnvelopeError::EmptyKey => "empty_key",
                    EnvelopeError::BadLeaseTtl(_) => "bad_lease_ttl",
                    EnvelopeError::Malformed(_) => "bad_envelope",
                };
                add(n, code, e.to_string());
                adapter_frames += 1;
                continue;
            }
        };
        let Some(env) = env else {
            adapter_frames += 1;
            continue;
        };

        match keys.get(&env.key) {
            Some(first) if first != &env => {
                add(
                    n,
                    "key_conflict",
                    format!(
                        "idempotency key {:?} reused with a different payload",
                        env.key
                    ),
                );
            }
            Some(_) => {}
            None => {
                keys.insert(env.key.clone(), env.clone());
            }
        }
        if let Some(lease) = &env.lease {
            match lease.op {
                LeaseVerb::Acquire => {
                    leases.insert(lease.id.clone());
                }
                LeaseVerb::Renew | LeaseVerb::Release if !leases.contains(&lease.id) => {
                    add(
                        n,
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
        match env.after_adapter_sequence {
            Some(anchor) => anchors.push((n, anchor)),
            None => adapter_frames += 1,
        }
    }

    for (n, anchor) in anchors {
        if anchor > adapter_frames {
            add(
                n,
                "sequence_beyond_stream",
                format!("after_adapter_sequence {anchor} but the stream has {adapter_frames} adapter frames"),
            );
        }
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

    fn update(meta: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"agent_message_chunk","_meta":{{"aoe":{meta}}}}}}}}}"#
        )
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
                    update(r#"{"v":1,"key":"a","parent_tool_call":"t"}"#),
                ],
                false,
                vec!["key_conflict"],
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
                "anchor reached",
                vec![
                    update(r#"{"v":1,"key":"a","after_adapter_sequence":1,"ephemeral":true}"#),
                    update(r#"{"v":1,"key":"b"}"#),
                ],
                false,
                vec![],
            ),
        ];
        for (name, stream, require_hello, expected) in cases {
            assert_eq!(run(&stream, require_hello), expected, "{name}");
        }
    }
}
