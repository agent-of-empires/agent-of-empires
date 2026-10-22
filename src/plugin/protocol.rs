//! The plugin worker protocol: newline-delimited JSON-RPC 2.0 over the

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const FORBIDDEN: i64 = -32001;
    pub const POLICY_DENIED: i64 = -32002;
    pub const CONFLICT: i64 = -32003;
    pub const RATE_LIMITED: i64 = -32004;
    pub const FAILED_PRECONDITION: i64 = -32005;
    pub const SERVICE_UNAVAILABLE: i64 = -32006;
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
}

impl RpcRequest {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    pub fn validate_envelope(&self) -> Result<&str, &'static str> {
        if self.jsonrpc.as_deref() != Some("2.0") {
            return Err("jsonrpc field must be \"2.0\"");
        }
        match self.method.as_deref() {
            Some(m) if !m.is_empty() => Ok(m),
            _ => Err("missing or empty \"method\""),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self::error_with_data(id, code, message, None)
    }

    pub fn error_with_data(
        id: Value,
        code: i64,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }

    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"serialize failed"}}"#
                .to_string()
        });
        line.push('\n');
        line
    }
}

pub fn parse_request(line: &str) -> Result<Option<RpcRequest>, serde_json::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed).map(Some)
}

/// A worker line answering a host-initiated request: an `id` and a `result` or `error`, no `method`.
pub struct WorkerReply {
    pub id: String,
    pub outcome: Result<Value, String>,
}

pub fn parse_worker_reply(line: &str) -> Option<WorkerReply> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = value.as_object()?;
    if obj.contains_key("method") {
        return None;
    }
    let id = obj.get("id")?.as_str()?.to_string();
    let outcome = match obj.get("error") {
        Some(error) => Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("worker error")
            .to_string()),
        None => Ok(obj.get("result")?.clone()),
    };
    Some(WorkerReply { id, outcome })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_request_skips_blanks_and_defers_envelope_errors() {
        let req = parse_request(r#"{"jsonrpc":"2.0","id":7,"method":"sessions.list","params":{}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(req.validate_envelope().unwrap(), "sessions.list");
        assert_eq!(req.id, Some(json!(7)));
        assert!(!req.is_notification());

        let req = parse_request(r#"{"jsonrpc":"2.0","method":"events.publish","params":{}}"#)
            .unwrap()
            .unwrap();
        assert!(req.is_notification());
        assert_eq!(req.validate_envelope().unwrap(), "events.publish");

        assert!(parse_request("   ").unwrap().is_none());
        assert!(parse_request("").unwrap().is_none());
        assert!(parse_request("{not json").is_err());

        for line in [
            r#"{"method":"sessions.list"}"#,
            r#"{"jsonrpc":"1.0","method":"sessions.list"}"#,
            r#"{"jsonrpc":"2.0"}"#,
        ] {
            let req = parse_request(line).unwrap().unwrap();
            assert!(req.validate_envelope().is_err(), "{line}");
        }
    }

    #[test]
    fn response_lines_are_single_ndjson() {
        let ok = RpcResponse::success(json!(1), json!({"ok": true})).to_line();
        assert!(ok.ends_with('\n'));
        assert_eq!(ok.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(ok.trim()).unwrap();
        assert_eq!(parsed["result"]["ok"], json!(true));
        assert_eq!(parsed["jsonrpc"], json!("2.0"));

        let err = RpcResponse::error(json!(2), codes::FORBIDDEN, "nope").to_line();
        let parsed: Value = serde_json::from_str(err.trim()).unwrap();
        assert_eq!(parsed["error"]["code"], json!(codes::FORBIDDEN));
        assert!(parsed.get("result").is_none());
    }

    #[test]
    fn worker_replies_are_told_apart_from_requests() {
        let ok =
            parse_worker_reply(r#"{"jsonrpc":"2.0","id":"h-1","result":{"drop":"*"}}"#).unwrap();
        assert_eq!(ok.id, "h-1");
        assert_eq!(ok.outcome.unwrap()["drop"], "*");

        let err = parse_worker_reply(
            r#"{"jsonrpc":"2.0","id":"h-2","error":{"code":-1,"message":"nope"}}"#,
        )
        .unwrap();
        assert_eq!(err.outcome.unwrap_err(), "nope");

        for not_a_reply in [
            r#"{"jsonrpc":"2.0","id":1,"method":"events.publish","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":7,"result":1}"#,
            "not json",
            "",
        ] {
            assert!(parse_worker_reply(not_a_reply).is_none(), "{not_a_reply}");
        }
    }
}
