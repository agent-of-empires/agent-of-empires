//! NDJSON framing and JSON-RPC line classification for the agent stdio stream.

use crate::acp::control_protocol::{self, PromptOutcome};
use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Decode once at framing acceptance. In particular, a duplicate correlation
/// field must not gain last-wins semantics through `serde_json::Value`.
struct NativeEnvelope(serde_json::Map<String, serde_json::Value>);

impl<'de> Deserialize<'de> for NativeEnvelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EnvelopeVisitor;
        impl<'de> serde::de::Visitor<'de> for EnvelopeVisitor {
            type Value = NativeEnvelope;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a native JSON-RPC object")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    let reserved = match key.as_str() {
                        "id" => Some("id"),
                        "method" => Some("method"),
                        "params" => Some("params"),
                        "result" => Some("result"),
                        "error" => Some("error"),
                        _ => None,
                    };
                    if let Some(field) = reserved.filter(|_| fields.contains_key(&key)) {
                        return Err(serde::de::Error::duplicate_field(field));
                    }
                    let value = map.next_value::<serde_json::Value>()?;
                    fields.insert(key, value);
                }
                Ok(NativeEnvelope(fields))
            }
        }
        deserializer.deserialize_map(EnvelopeVisitor)
    }
}

pub(super) enum NativeMessage {
    Request {
        agent_id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
    Response {
        id: i64,
        value: serde_json::Value,
    },
}

pub(super) fn parse_native_message(line: &[u8]) -> Option<NativeMessage> {
    let NativeEnvelope(mut fields) = serde_json::from_slice(line).ok()?;
    match fields.get("method") {
        Some(serde_json::Value::String(_)) => {
            let serde_json::Value::String(method) = fields.remove("method")? else {
                unreachable!()
            };
            let params = fields.remove("params").unwrap_or(serde_json::Value::Null);
            match fields.remove("id").filter(|id| !id.is_null()) {
                Some(agent_id) => Some(NativeMessage::Request {
                    agent_id,
                    method,
                    params,
                }),
                None => Some(NativeMessage::Notification { method, params }),
            }
        }
        Some(serde_json::Value::Null) | None => {
            let id = fields.get("id")?.as_i64()?;
            Some(NativeMessage::Response {
                id,
                value: serde_json::Value::Object(fields),
            })
        }
        _ => None,
    }
}

/// Missing result/error remains a null generic result, as in the existing
/// set-mode transport. A malformed error stays unknown, never a fake success.
pub(super) fn parse_agent_call_outcome(
    mut value: serde_json::Value,
) -> Result<Result<serde_json::Value, control_protocol::JsonRpcError>, serde_json::Error> {
    if let Some(error) = value
        .get_mut("error")
        .map(std::mem::take)
        .filter(|error| !error.is_null())
    {
        return Ok(Err(serde_json::from_value(error)?));
    }
    Ok(Ok(value
        .get_mut("result")
        .map(std::mem::take)
        .unwrap_or(serde_json::Value::Null)))
}

pub(super) const MAX_FRAME_BYTES: usize = control_protocol::MAX_AGENT_FRAME_BYTES;

pub(super) async fn read_frame_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len()); // EOF (buf holds any final unterminated bytes)
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |pos| pos + 1);
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if buf.len() > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ndjson frame exceeds MAX_FRAME_BYTES",
            ));
        }
        if newline.is_some() {
            return Ok(buf.len());
        }
    }
}

#[derive(Deserialize)]
pub(super) struct JsonRpcErrorPeek {
    #[serde(default)]
    pub(super) code: i32,
    #[serde(default)]
    pub(super) message: String,
    #[serde(default)]
    pub(super) data: Option<serde_json::Value>,
}

pub(super) fn parse_prompt_outcome(
    mut value: serde_json::Value,
) -> Result<PromptOutcome, serde_json::Error> {
    if let Some(error) = value
        .get_mut("error")
        .map(std::mem::take)
        .filter(|error| !error.is_null())
    {
        let error: JsonRpcErrorPeek = serde_json::from_value(error)?;
        return Ok(PromptOutcome::Error {
            code: error.code,
            message: error.message,
            data: error.data,
        });
    }
    let stop_reason = value
        .get_mut("result")
        .and_then(|result| result.get_mut("stopReason"))
        .map(std::mem::take)
        .and_then(|reason| match reason {
            serde_json::Value::String(reason) => Some(reason),
            _ => None,
        });
    Ok(PromptOutcome::Completed { stop_reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_request_preserves_id_shape() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        type Case = (&'static [u8], Option<(serde_json::Value, String)>);
        let cases: Vec<Case> = vec![
            (
                br#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{}}"#,
                Some((j("7"), "fs/read_text_file".into())),
            ),
            (
                br#"{"jsonrpc":"2.0","id":"abc","method":"terminal/create","params":{}}"#,
                Some((j(r#""abc""#), "terminal/create".into())),
            ),
            (
                br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
                None,
            ),
            (br#"{"jsonrpc":"2.0","id":7,"result":{}}"#, None),
            (b"not json", None),
        ];
        for (line, expected) in cases {
            assert_eq!(
                parse_native_message(line).and_then(|message| match message {
                    NativeMessage::Request {
                        agent_id, method, ..
                    } => Some((agent_id, method)),
                    _ => None,
                }),
                expected,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn native_responses_classify_lines() {
        let completed = |reason: &str| PromptOutcome::Completed {
            stop_reason: Some(reason.into()),
        };
        let cases: [(&[u8], Option<i64>, Option<(i64, PromptOutcome)>); 9] = [
            (
                br#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#,
                Some(3),
                Some((3, completed("end_turn"))),
            ),
            (
                br#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"boom","data":{"errorKind":"rate_limit"}}}"#,
                Some(4),
                Some((
                    4,
                    PromptOutcome::Error {
                        code: -32000,
                        message: "boom".into(),
                        data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                    },
                )),
            ),
            (br#"{"jsonrpc":"2.0","id":42,"method":"foo"}"#, None, None),
            (
                br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
                None,
                None,
            ),
            (br#"{"jsonrpc":"2.0","id":"x","result":{}}"#, None, None),
            (b"not json", None, None),
            (br#"{"id":3,"id":4,"result":{}}"#, None, None),
            (br#"{"id":3,"method":null,"method":null,"result":{}}"#, None, None),
            (br#"{"id":3,"result":{},"result":{"stopReason":"end_turn"}}"#, None, None),
        ];
        for (line, id, response) in cases {
            let text = String::from_utf8_lossy(line);
            let parsed = parse_native_message(line);
            let actual_id = match &parsed {
                Some(NativeMessage::Response { id, .. }) => Some(*id),
                _ => None,
            };
            assert_eq!(actual_id, id, "{text}");
            let actual_response = parsed.and_then(|message| match message {
                NativeMessage::Response { id, value } => Some((
                    id,
                    parse_prompt_outcome(value).expect("canonical prompt outcome"),
                )),
                _ => None,
            });
            assert_eq!(actual_response, response, "{text}");
        }
    }

    #[test]
    fn malformed_native_error_is_unknown_for_prompt_and_agent_calls() {
        for line in [
            br#"{"id":3,"error":"not an error object"}"#.as_slice(),
            br#"{"id":3,"error":{"code":"not a number","message":"failure"}}"#.as_slice(),
        ] {
            let Some(NativeMessage::Response { value, .. }) = parse_native_message(line) else {
                panic!("numeric original response remains correlated");
            };
            assert!(parse_prompt_outcome(value.clone()).is_err());
            assert!(parse_agent_call_outcome(value).is_err());
        }
    }
}
