//! Session create / turn-delivery DTOs for the `sessions.create` and
//! `sessions.turn.send` worker RPCs (API v9, #2897).
//!
//! The host validates every field against its own catalogs and policy; a
//! plugin cannot pick a view other than structured, pre-approve repository
//! trust, or pass agent launch flags. The caller's plugin identity comes
//! from the RPC connection, never from these payloads.

use serde::{Deserialize, Serialize};

/// Parameters of `sessions.create`. Requires the `session.create` grant;
/// additionally `session.prompt` when `initial_turn` is present and
/// `session.unattended` when the host classifies `mode_id` as unattended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsCreateRequest {
    /// Agent to run, an id from `acp.capabilities.get`.
    pub agent_id: String,
    /// Project directory the session runs in. Canonicalized and checked
    /// against repository trust by the host, fail-closed. Absent or empty
    /// means *no project*: the host provisions a throwaway scratch session
    /// (no repo, so no trust anchor). Optional since API v11; a present value
    /// keeps the v9/v10 behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// Additional repository paths for a multi-repo session, each canonicalized
    /// and existence-checked by the host. Only valid alongside a
    /// `project_path` (the first repo is the trust anchor); combining extras
    /// with a scratch session is refused. API v11.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_project_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Approval mode id. Omitted means the adapter default (interactive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// First prompt, accepted atomically with the create and delivered once
    /// the worker is live (at-least-once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_turn: Option<InitialTurn>,
    /// Create-deduplication key, scoped to the calling plugin. Retrying with
    /// the same key and payload returns the existing session
    /// (`created: false`); a different payload under the same key is a
    /// conflict. Retained while the session record exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// The initial prompt of a created session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialTurn {
    pub text: String,
}

/// Response of `sessions.create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsCreateResponse {
    pub session_id: String,
    /// `false` when an existing session was returned by idempotency.
    pub created: bool,
}

/// Parameters of `sessions.turn.send`. Requires the `session.prompt` grant;
/// the target must have been created by the calling plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnSendRequest {
    pub session_id: String,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_wire_fixture_is_stable() {
        let request = SessionsCreateRequest {
            agent_id: "claude".into(),
            project_path: Some("/home/user/project".into()),
            extra_project_paths: Vec::new(),
            model_id: Some("sonnet".into()),
            mode_id: Some("plan".into()),
            title: Some("nightly maintenance".into()),
            group: None,
            initial_turn: Some(InitialTurn {
                text: "Run the nightly task".into(),
            }),
            idempotency_key: Some("job-1:2026-07-16T03:00:00Z".into()),
        };
        let json = serde_json::to_value(&request).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "agent_id": "claude",
                "project_path": "/home/user/project",
                "model_id": "sonnet",
                "mode_id": "plan",
                "title": "nightly maintenance",
                "initial_turn": {"text": "Run the nightly task"},
                "idempotency_key": "job-1:2026-07-16T03:00:00Z"
            })
        );
        let round: SessionsCreateRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round, request);
    }

    #[test]
    fn create_request_rejects_bypass_flags() {
        // No unknown field can smuggle a host-side knob (allow_untrusted,
        // extra args, env) through the create payload.
        let err = serde_json::from_value::<SessionsCreateRequest>(serde_json::json!({
            "agent_id": "claude",
            "project_path": "/p",
            "allow_untrusted": true
        }))
        .expect_err("unknown fields must be rejected");
        assert!(err.to_string().contains("allow_untrusted"));
    }

    #[test]
    fn scratch_request_omits_project_path() {
        // No project selected: project_path is absent on the wire (scratch),
        // and an omitted project_path round-trips to None.
        let request = SessionsCreateRequest {
            agent_id: "claude".into(),
            project_path: None,
            extra_project_paths: Vec::new(),
            model_id: None,
            mode_id: None,
            title: None,
            group: None,
            initial_turn: None,
            idempotency_key: None,
        };
        let json = serde_json::to_value(&request).expect("serialize");
        assert!(json.get("project_path").is_none());
        assert!(json.get("extra_project_paths").is_none());
        let round: SessionsCreateRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round, request);
    }

    #[test]
    fn multi_repo_request_carries_extra_paths() {
        let request = SessionsCreateRequest {
            agent_id: "claude".into(),
            project_path: Some("/repos/app".into()),
            extra_project_paths: vec!["/repos/lib".into(), "/repos/proto".into()],
            model_id: None,
            mode_id: None,
            title: None,
            group: None,
            initial_turn: None,
            idempotency_key: None,
        };
        let json = serde_json::to_value(&request).expect("serialize");
        assert_eq!(
            json["extra_project_paths"],
            serde_json::json!(["/repos/lib", "/repos/proto"])
        );
        let round: SessionsCreateRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round, request);
    }
}
