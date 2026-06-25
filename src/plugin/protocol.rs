//! The plugin worker protocol: newline-delimited JSON-RPC 2.0 over the
//! worker's stdio.
//!
//! A worker is the JSON-RPC client: it writes one request object per line to
//! its stdout and reads one response object per line on its stdin. The host
//! is the server. This is the language-agnostic wire contract; any executable
//! that speaks it is a valid worker, which is why the host resolves and
//! launches workers of different runtime kinds (see [`crate::plugin::launch`])
//! through the same path.
//!
//! Notifications (no `id`) are accepted but produce no response. Anything the
//! worker writes to stderr is never protocol; it is drained to the worker log.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC error codes. The negative range below `-32000` is reserved by the
/// spec for implementation-defined server errors; [`FORBIDDEN`] is ours, for a
/// method whose capability the plugin did not declare or was not granted.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Capability not declared or not granted for the calling plugin.
    pub const FORBIDDEN: i64 = -32001;
}

/// One inbound request from a worker. `id` is absent for a notification.
///
/// Every field is optional at the serde layer so that any well-formed JSON
/// object deserializes; the JSON-RPC 2.0 envelope is then validated by
/// [`RpcRequest::validate_envelope`]. This keeps a parse failure meaning
/// "malformed JSON" (PARSE_ERROR) and a bad request shape meaning
/// "invalid request" (INVALID_REQUEST), rather than conflating the two.
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
    /// A request with no `id` is a notification: the host must not answer it.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// Validate the JSON-RPC 2.0 envelope, returning the method name on success.
    /// `jsonrpc` must be exactly `"2.0"` and `method` must be present and
    /// non-empty; otherwise the request is well-formed JSON but not a valid
    /// request, which the host reports as `INVALID_REQUEST`.
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

/// One outbound response to a worker. Exactly one of `result` / `error` is set.
#[derive(Debug, Clone, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
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
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }

    /// Serialize to a single ndjson line including the trailing newline.
    pub fn to_line(&self) -> String {
        // Serializing a plain struct of JSON values cannot fail.
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"serialize failed"}}"#
                .to_string()
        });
        line.push('\n');
        line
    }
}

/// Parse one ndjson line into a request. An empty or whitespace-only line is
/// `Ok(None)` (skipped); malformed JSON is an error the caller reports as a
/// parse error and treats as fatal to the worker.
pub fn parse_request(line: &str) -> Result<Option<RpcRequest>, serde_json::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_request_round_trip() {
        let req = parse_request(r#"{"jsonrpc":"2.0","id":7,"method":"sessions.list","params":{}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(req.validate_envelope().unwrap(), "sessions.list");
        assert_eq!(req.id, Some(json!(7)));
        assert!(!req.is_notification());
    }

    #[test]
    fn notification_has_no_id() {
        let req = parse_request(r#"{"jsonrpc":"2.0","method":"events.publish","params":{}}"#)
            .unwrap()
            .unwrap();
        assert!(req.is_notification());
        assert_eq!(req.validate_envelope().unwrap(), "events.publish");
    }

    #[test]
    fn blank_line_is_skipped() {
        assert!(parse_request("   ").unwrap().is_none());
        assert!(parse_request("").unwrap().is_none());
    }

    #[test]
    fn malformed_line_is_error() {
        assert!(parse_request("{not json").is_err());
    }

    #[test]
    fn well_formed_json_with_bad_envelope_is_not_a_parse_error() {
        // Missing jsonrpc: parses fine, but the envelope is invalid.
        let req = parse_request(r#"{"method":"sessions.list"}"#)
            .unwrap()
            .unwrap();
        assert!(req.validate_envelope().is_err());
        // Wrong jsonrpc version.
        let req = parse_request(r#"{"jsonrpc":"1.0","method":"sessions.list"}"#)
            .unwrap()
            .unwrap();
        assert!(req.validate_envelope().is_err());
        // Missing method.
        let req = parse_request(r#"{"jsonrpc":"2.0"}"#).unwrap().unwrap();
        assert!(req.validate_envelope().is_err());
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
}
