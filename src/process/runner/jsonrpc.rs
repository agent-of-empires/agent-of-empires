//! NDJSON framing and JSON-RPC line classification for the agent stdio stream.

use crate::acp::control_protocol::{self, PromptOutcome};
use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

#[derive(Deserialize)]
pub(super) struct JsonRpcPeek {
    #[serde(default)]
    pub(super) id: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) method: Option<String>,
}

/// The id is returned as its JSON value so the response echoes it verbatim (number vs string).
pub(super) fn parse_request_value_id(line: &[u8]) -> Option<(serde_json::Value, String)> {
    let peek: JsonRpcPeek = serde_json::from_slice(line).ok()?;
    let id = peek.id?;
    let method = peek.method?;
    Some((id, method))
}

pub(super) fn parse_notification(line: &[u8]) -> Option<(String, serde_json::Value)> {
    let mut value: serde_json::Value = serde_json::from_slice(line).ok()?;
    if value.get("id").is_some_and(|id| !id.is_null()) {
        return None;
    }
    let method = value.get("method")?.as_str()?.to_string();
    let params = value
        .get_mut("params")
        .map(std::mem::take)
        .unwrap_or(serde_json::Value::Null);
    Some((method, params))
}

/// A response with neither result nor error is a null result (`session/set_mode` answers `{}`).
pub(super) fn parse_agent_call_outcome(
    line: &[u8],
) -> Result<serde_json::Value, control_protocol::JsonRpcError> {
    let mut value: serde_json::Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => {
            return Err(control_protocol::JsonRpcError::new(
                control_protocol::DAEMON_GONE,
                format!("agent response was not JSON: {e}"),
            ))
        }
    };
    if let Some(error) = value.get_mut("error").map(std::mem::take) {
        if !error.is_null() {
            return Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| {
                control_protocol::JsonRpcError::new(
                    control_protocol::DAEMON_GONE,
                    format!("agent error envelope was malformed: {error}"),
                )
            }));
        }
    }
    Ok(value
        .get_mut("result")
        .map(std::mem::take)
        .unwrap_or(serde_json::Value::Null))
}

pub(super) fn parse_response_id(line: &[u8]) -> Option<i64> {
    let peek: JsonRpcPeek = serde_json::from_slice(line).ok()?;
    if peek.method.is_some() {
        return None;
    }
    peek.id?.as_i64()
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
pub(super) struct JsonRpcResponsePeek {
    #[serde(default)]
    pub(super) id: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) method: Option<String>,
    #[serde(default)]
    pub(super) result: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) error: Option<JsonRpcErrorPeek>,
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

pub(super) fn parse_response(line: &[u8]) -> Option<(i64, PromptOutcome)> {
    let peek: JsonRpcResponsePeek = serde_json::from_slice(line).ok()?;
    if peek.method.is_some() {
        return None;
    }
    let id = peek.id?.as_i64()?;
    let outcome = if let Some(err) = peek.error {
        PromptOutcome::Error {
            code: err.code,
            message: err.message,
            data: err.data,
        }
    } else {
        let stop_reason = peek
            .result
            .as_ref()
            .and_then(|r| r.get("stopReason"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        PromptOutcome::Completed { stop_reason }
    };
    Some((id, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_value_id_preserves_id_shape() {
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
                parse_request_value_id(line),
                expected,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn response_parsers_classify_lines() {
        let completed = |reason: &str| PromptOutcome::Completed {
            stop_reason: Some(reason.into()),
        };
        let cases: [(&[u8], Option<i64>, Option<(i64, PromptOutcome)>); 6] = [
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
        ];
        for (line, id, response) in cases {
            let text = String::from_utf8_lossy(line);
            assert_eq!(parse_response_id(line), id, "{text}");
            assert_eq!(parse_response(line), response, "{text}");
        }
    }
}
