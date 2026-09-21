//! Session create / turn-delivery DTOs for the `sessions.create` and

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsCreateRequest {
    /// Agent to run, an id from `acp.capabilities.get`.
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_project_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sandbox: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Approval mode id. Omitted means the adapter default (interactive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_turn: Option<InitialTurn>,
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
            sandbox: false,
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
        let request = SessionsCreateRequest {
            agent_id: "claude".into(),
            project_path: None,
            extra_project_paths: Vec::new(),
            sandbox: false,
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
    fn sandbox_flag_serializes_only_when_set() {
        let mut request = SessionsCreateRequest {
            agent_id: "claude".into(),
            project_path: Some("/p".into()),
            extra_project_paths: Vec::new(),
            sandbox: false,
            model_id: None,
            mode_id: None,
            title: None,
            group: None,
            initial_turn: None,
            idempotency_key: None,
        };
        assert!(serde_json::to_value(&request)
            .expect("serialize")
            .get("sandbox")
            .is_none());
        request.sandbox = true;
        let json = serde_json::to_value(&request).expect("serialize");
        assert_eq!(json["sandbox"], serde_json::json!(true));
        let round: SessionsCreateRequest = serde_json::from_value(json).expect("deserialize");
        assert!(round.sandbox);
    }

    #[test]
    fn multi_repo_request_carries_extra_paths() {
        let request = SessionsCreateRequest {
            agent_id: "claude".into(),
            project_path: Some("/repos/app".into()),
            extra_project_paths: vec!["/repos/lib".into(), "/repos/proto".into()],
            sandbox: false,
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
