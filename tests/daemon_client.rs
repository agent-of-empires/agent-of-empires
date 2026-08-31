use std::collections::HashMap;
use std::time::Duration;

use agent_of_empires::daemon::{
    AcpWorkerState, DaemonClient, DaemonClientError, PromptAttachmentKind,
};
use agent_of_empires::session::SessionScope;
use reqwest::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug)]
struct RecordedRequest {
    target: String,
    headers: HashMap<String, String>,
}

fn response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut wire = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        wire.push_str(name);
        wire.push_str(": ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    wire.push_str("\r\n");
    wire.push_str(body);
    wire.into_bytes()
}

fn structured_success() -> String {
    serde_json::json!({
        "sessions": [{
            "id": "session-a",
            "title": "Structured session",
            "project_path": "/tmp/project",
            "artifact_dir": "/tmp/artifacts",
            "group_path": "",
            "tool": "claude",
            "status": "Running",
            "dormant": false,
            "yolo_mode": false,
            "created_at": "2026-01-01T00:00:00Z",
            "is_sandboxed": false,
            "scratch": false,
            "favorited": false,
            "urgent": false,
            "has_managed_worktree": true,
            "has_cleanable_worktree": true,
            "has_terminal": false,
            "profile": "default",
            "cleanup_defaults": {
                "delete_worktree": true,
                "delete_branch": false,
                "delete_sandbox": true,
                "delete_to_trash": true
            },
            "view": "structured",
            "acp_worker_state": "running",
            "acp_capable": true,
            "queued_prompts": [{
                "id": "prompt-a",
                "seq": 7,
                "text": "continue",
                "attachments": [{
                    "id": "attachment-a",
                    "kind": "image",
                    "mime_type": "image/png",
                    "size": 42
                }],
                "created_at": "2026-01-01T00:01:00Z",
                "origin_device": "laptop"
            }],
            "acp_session_id": "acp-session-a",
            "acp_agent": "claude",
            "acp_can_fork": true,
            "keeps_context": true,
            "clear_aliases": ["/clear"],
            "claude_fullscreen": false,
            "workspace_repos": [{
                "name": "project",
                "source_path": "/tmp/project",
                "branch": "feature"
            }]
        }],
        "workspace_ordering": ["workspace-a"]
    })
    .to_string()
}

async fn serve_once(wire_response: Vec<u8>) -> (String, tokio::task::JoinHandle<RecordedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let _ = stream.write_all(&wire_response).await;

        let request = String::from_utf8(request).unwrap();
        let mut lines = request.split("\r\n");
        let target = lines
            .next()
            .unwrap()
            .split_ascii_whitespace()
            .nth(1)
            .unwrap()
            .to_string();
        let headers = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        RecordedRequest { target, headers }
    });
    (format!("http://{address}"), task)
}

#[tokio::test]
async fn daemon_client_http_contract() {
    fn assert_traits<T: Clone + Send + Sync>() {}
    assert_traits::<DaemonClient>();

    let success = r#"{"sessions":[],"workspace_ordering":["workspace-a"]}"#;
    let cases = [
        ("", None, None, "/api/sessions"),
        (
            "/",
            Some(SessionScope::Live),
            Some("opaque-token"),
            "/api/sessions?state=live",
        ),
        (
            "/prefix",
            Some(SessionScope::Trashed),
            Some("opaque-token"),
            "/prefix/api/sessions?state=trashed",
        ),
        (
            "/prefix/",
            Some(SessionScope::All),
            None,
            "/prefix/api/sessions?state=all",
        ),
    ];

    for (suffix, state, token, expected_target) in cases {
        let (origin, request) = serve_once(response("200 OK", &[], success)).await;
        let base_url = format!("{origin}{suffix}");
        let client = DaemonClient::new(&base_url, token).unwrap();
        let debug = format!("{client:?}");
        if let Some(token) = token {
            assert!(!debug.contains(token));
        }

        let envelope = client.list_sessions(state).await.unwrap();
        assert!(envelope.sessions.is_empty());
        assert_eq!(envelope.workspace_ordering, ["workspace-a"]);
        let request = request.await.unwrap();
        assert_eq!(request.target, expected_target);
        let expected_authorization = token.map(|token| format!("Bearer {token}"));
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            expected_authorization.as_deref()
        );
        assert!(!request.target.contains("opaque-token"));
    }

    for invalid in [
        "ftp://example.test",
        "http://user@example.test",
        "http://example.test?mode=bad",
        "http://example.test#fragment",
    ] {
        assert!(matches!(
            DaemonClient::new(invalid, None),
            Err(DaemonClientError::InvalidBaseUrl { .. })
        ));
    }
    assert!(matches!(
        DaemonClient::new("http://example.test", Some("bad\nvalue")),
        Err(DaemonClientError::InvalidBearerToken)
    ));

    for invalid_token in ["bad value", "tøken"] {
        assert!(matches!(
            DaemonClient::new("http://example.test", Some(invalid_token)),
            Err(DaemonClientError::InvalidBearerToken)
        ));
    }

    let structured = structured_success();
    let (origin, request) = serve_once(response("200 OK", &[], &structured)).await;
    let envelope = DaemonClient::new(&origin, None)
        .unwrap()
        .list_sessions(None)
        .await
        .unwrap();
    request.await.unwrap();
    let session = &envelope.sessions[0];
    assert_eq!(session.view, agent_of_empires::session::View::Structured);
    assert_eq!(session.acp_worker_state, AcpWorkerState::Running);
    assert!(session.acp_capable);
    assert!(session.acp_can_fork);
    assert!(session.keeps_context);
    assert_eq!(session.clear_aliases, ["/clear"]);
    assert_eq!(session.queued_prompts[0].seq, 7);
    assert_eq!(
        session.queued_prompts[0].attachments[0].kind,
        PromptAttachmentKind::Image
    );
    let round_trip = serde_json::to_value(&envelope).unwrap();
    for key in [
        "view",
        "acp_worker_state",
        "acp_capable",
        "queued_prompts",
        "acp_session_id",
        "acp_agent",
        "acp_can_fork",
        "keeps_context",
        "clear_aliases",
    ] {
        assert!(
            round_trip["sessions"][0].get(key).is_some(),
            "missing {key}"
        );
    }

    let oversized_success = response("200 OK", &[], &"x".repeat(16_777_217));
    let (origin, request) = serve_once(oversized_success).await;
    assert!(matches!(
        DaemonClient::new(&origin, None)
            .unwrap()
            .list_sessions(None)
            .await,
        Err(DaemonClientError::ResponseTooLarge { limit: 16_777_216 })
    ));
    request.await.unwrap();

    let secret = "secret-status-token";
    let oversized = format!("{secret}{}", "x".repeat(10_000));
    let (origin, request) = serve_once(response("503 Service Unavailable", &[], &oversized)).await;
    let result = DaemonClient::new(&origin, Some(secret))
        .unwrap()
        .list_sessions(None)
        .await;
    let error = match result {
        Ok(_) => panic!("expected status error"),
        Err(error) => error,
    };
    let error_text = error.to_string();
    match error {
        DaemonClientError::Status {
            status,
            body,
            truncated,
        } => {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(truncated);
            assert!(body.is_empty());
            assert!(!error_text.contains(secret));
        }
        other => panic!("expected status error, got {other:?}"),
    }
    request.await.unwrap();

    let escaped_cases = [
        (
            "secret/token",
            r#"secret\/token"#,
            r#"{"error":"secret\/token"}"#,
        ),
        (
            r#"secret"token"#,
            r#"secret\"token"#,
            r#"{"error":"secret\"token"}"#,
        ),
        (
            r#"secret\token"#,
            r#"secret\\token"#,
            r#"{"error":"secret\\token"}"#,
        ),
        ("secret/a/b", r#"secret\/a/b"#, r#"{"error":"secret\/a/b"}"#),
        ("secret", r#"\u0073ecret"#, r#"{"error":"\u0073ecret"}"#),
        ("ab", "ab", "aaaaabbbbb"),
        (
            "secret-status-token",
            "secret-status-token",
            "secret-status-tokesecret-status-tokesecret-status-tokesecret-status-tokennnn",
        ),
    ];
    for (token, encoded_token, response_body) in escaped_cases {
        let (origin, request) = serve_once(response("401 Unauthorized", &[], response_body)).await;
        let error = match DaemonClient::new(&origin, Some(token))
            .unwrap()
            .list_sessions(None)
            .await
        {
            Ok(_) => panic!("expected status error"),
            Err(error) => error,
        };
        let error_text = error.to_string();
        let error_debug = format!("{error:?}");
        let DaemonClientError::Status { body, .. } = &error else {
            panic!("expected status error, got {error:?}");
        };
        assert!(body.is_empty());
        for exposed in [token, encoded_token] {
            assert!(!body.contains(exposed));
            assert!(!error_text.contains(exposed));
            assert!(!error_debug.contains(exposed));
        }
        request.await.unwrap();
    }

    let marker_token = "<redacted>";
    let (origin, request) = serve_once(response("401 Unauthorized", &[], marker_token)).await;
    let error = match DaemonClient::new(&origin, Some(marker_token))
        .unwrap()
        .list_sessions(None)
        .await
    {
        Ok(_) => panic!("expected status error"),
        Err(error) => error,
    };
    let error_text = error.to_string();
    let error_debug = format!("{error:?}");
    let DaemonClientError::Status { body, .. } = &error else {
        panic!("expected status error, got {error:?}");
    };
    assert!(body.is_empty());
    for output in [body.as_str(), error_text.as_str(), error_debug.as_str()] {
        assert!(!output.contains(marker_token));
    }
    request.await.unwrap();

    let overlap_token = "a-super-secret/a";
    let exposed_prefix = "a-super-secret/";
    let mut overlap_body = "x".repeat(8 * 1024 - overlap_token.len());
    overlap_body.push_str(overlap_token);
    overlap_body.push_str("truncated tail");
    let (origin, request) = serve_once(response("401 Unauthorized", &[], &overlap_body)).await;
    let error = match DaemonClient::new(&origin, Some(overlap_token))
        .unwrap()
        .list_sessions(None)
        .await
    {
        Ok(_) => panic!("expected status error"),
        Err(error) => error,
    };
    let error_text = error.to_string();
    let error_debug = format!("{error:?}");
    let DaemonClientError::Status {
        body, truncated, ..
    } = &error
    else {
        panic!("expected status error, got {error:?}");
    };
    assert!(*truncated);
    assert!(body.is_empty());
    for output in [body.as_str(), error_text.as_str(), error_debug.as_str()] {
        assert!(!output.contains(overlap_token));
        assert!(!output.contains(exposed_prefix));
    }
    request.await.unwrap();

    let diagnostic = "plain unauthenticated diagnostic";
    let (origin, request) = serve_once(response("400 Bad Request", &[], diagnostic)).await;
    let error = match DaemonClient::new(&origin, None)
        .unwrap()
        .list_sessions(None)
        .await
    {
        Ok(_) => panic!("expected status error"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(diagnostic));
    let DaemonClientError::Status {
        status,
        body,
        truncated,
    } = error
    else {
        panic!("expected status error");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, diagnostic);
    assert!(!truncated);
    request.await.unwrap();

    let (origin, request) = serve_once(response("200 OK", &[], "{not-json")).await;
    assert!(matches!(
        DaemonClient::new(&origin, None)
            .unwrap()
            .list_sessions(None)
            .await,
        Err(DaemonClientError::Decode(_))
    ));
    request.await.unwrap();

    let decode_secret = "decode-secret-token";
    let decode_bodies = [
        format!(r#""{decode_secret}""#),
        r#""\u0064ecode-secret-token""#.to_owned(),
    ];
    for decode_body in decode_bodies {
        let (origin, request) = serve_once(response("200 OK", &[], &decode_body)).await;
        let error = match DaemonClient::new(&origin, Some(decode_secret))
            .unwrap()
            .list_sessions(None)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("expected decode error"),
        };
        assert!(matches!(&error, DaemonClientError::AuthenticatedDecode));
        assert!(!error.to_string().contains(decode_secret));
        assert!(!format!("{error:?}").contains(decode_secret));
        assert!(std::error::Error::source(&error).is_none());
        request.await.unwrap();
    }

    let (origin, request) = serve_once(response(
        "302 Found",
        &[("Location", "http://example.invalid/elsewhere")],
        "redirect",
    ))
    .await;
    assert!(matches!(
        DaemonClient::new(&origin, Some("redirect-token"))
            .unwrap()
            .list_sessions(None)
            .await,
        Err(DaemonClientError::Status {
            status: StatusCode::FOUND,
            ..
        })
    ));
    request.await.unwrap();

    let (redirect_origin, mut redirected_request) =
        serve_once(response("200 OK", &[], success)).await;
    let location = format!("{redirect_origin}/redirected");
    let (origin, first_request) = serve_once(response(
        "302 Found",
        &[("Location", &location)],
        "redirect",
    ))
    .await;
    assert!(matches!(
        DaemonClient::new(&origin, Some("redirect-token"))
            .unwrap()
            .list_sessions(None)
            .await,
        Err(DaemonClientError::Status {
            status: StatusCode::FOUND,
            ..
        })
    ));
    assert_eq!(
        first_request
            .await
            .unwrap()
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer redirect-token")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut redirected_request)
            .await
            .is_err()
    );
    redirected_request.abort();
}
