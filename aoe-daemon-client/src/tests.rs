use super::*;
use http_body_util::{BodyExt, Full};
use hyper::{
    body::{Bytes, Incoming},
    Request, Response,
};
use std::{convert::Infallible, future::Future};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;

type TestResponse = Response<Full<Bytes>>;

fn reply(
    status: u16,
    headers: impl IntoIterator<Item = (&'static str, &'static str)>,
    body: &'static str,
) -> TestResponse {
    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        response = response.header(name, value);
    }
    response
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .unwrap()
}

fn serve_stream<S, H, F>(stream: S, handler: H) -> JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: FnOnce(Request<Incoming>) -> F + Send + 'static,
    F: Future<Output = TestResponse> + Send + 'static,
{
    let handler = std::sync::Mutex::new(Some(handler));
    tokio::spawn(async move {
        let service = hyper::service::service_fn(move |request| {
            let handler = handler
                .lock()
                .unwrap()
                .take()
                .expect("one request per fixture connection");
            async move { Ok::<_, Infallible>(handler(request).await) }
        });
        hyper::server::conn::http1::Builder::new()
            .keep_alive(false)
            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
            .await
            .unwrap();
    })
}

async fn serve_tcp<H, F>(handler: H) -> (String, JoinHandle<()>)
where
    H: FnOnce(Request<Incoming>) -> F + Send + 'static,
    F: Future<Output = TestResponse> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_stream(stream, handler).await.unwrap();
    });
    (base, server)
}

#[cfg(unix)]
pub(crate) fn verify_test_peer(
    stream: &tokio::net::UnixStream,
    _: Option<u32>,
) -> Result<(), DaemonClientError> {
    stream
        .peer_cred()
        .map(|_| ())
        .map_err(|_| DaemonClientError::PeerIdentity)
}

#[cfg(unix)]
pub(crate) fn unix_client(
    path: impl Into<std::path::PathBuf>,
) -> Result<DaemonClient, DaemonClientError> {
    DaemonClient::new_unix(UnixTransport::new(path, verify_test_peer))
}

#[cfg(unix)]
async fn serve_unix<H, F>(path: &std::path::Path, handler: H) -> JoinHandle<()>
where
    H: FnOnce(Request<Incoming>) -> F + Send + 'static,
    F: Future<Output = TestResponse> + Send + 'static,
{
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_stream(stream, handler).await.unwrap();
    })
}

async fn serve_mutation_once(
    route: &'static str,
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
    tx: tokio::sync::oneshot::Sender<CapturedRequest>,
) -> String {
    serve_tcp(move |request| async move {
        let (parts, incoming) = request.into_parts();
        let path = parts.uri.path();
        let segments = path.split('/');
        let route = route.split('/');
        assert_eq!(segments.clone().count(), route.clone().count());
        assert!(
            segments
                .zip(route)
                .all(|(got, want)| got == want || want == "{id}"),
            "unexpected request path: {path}"
        );
        let bytes = incoming.collect().await.unwrap().to_bytes();
        let _ = tx.send(CapturedRequest {
            method: parts.method.to_string(),
            path: path.to_string(),
            epoch: parts
                .headers
                .get(RUNTIME_EPOCH_HEADER)
                .map(|value| value.to_str().unwrap().to_owned()),
            body: bytes.to_vec(),
        });
        reply(status, headers, body)
    })
    .await
    .0
}

#[test]
fn default_profile_is_the_row_the_daemon_marks() {
    let rows: Vec<ProfileRow> = serde_json::from_str(
        r#"[{"name":"main","is_default":false},{"name":"work","is_default":true,"description":"x"}]"#,
    )
    .unwrap();
    assert_eq!(default_of(rows).as_deref(), Some("work"));
    assert_eq!(default_of(Vec::new()), None);
}

#[test]
fn creation_failure_discloses_only_the_public_contract_remotely() {
    let private = r#"{"error":"validation_failed","message":"private configured command SECRET","stage":"SECRET","reference":"SECRET"}"#;
    let remote = public_creation_failure(private, false);
    assert!(!remote.contains("SECRET"));
    assert!(public_creation_failure(private, true).contains("private configured command"));
    assert!(!public_creation_failure("raw stderr SECRET", false).contains("SECRET"));
    for (public, message, stage) in [
        (
            r#"{"error":"hook_failed","message":"An on_create hook exited with code 7.","stage":"running_hooks","reference":"creation-ref"}"#,
            "exited with code 7",
            "running_hooks",
        ),
        (
            r#"{"error":"sandbox_image_update_failed","message":"Couldn't download the sandbox image: not enough disk space.","stage":"pulling_image","reference":"creation-ref"}"#,
            "Couldn't download the sandbox image: not enough disk space.",
            "pulling_image",
        ),
    ] {
        let rendered = public_creation_failure(public, false);
        assert!(rendered.contains(message), "{rendered}");
        assert!(rendered.contains(stage), "{rendered}");
        assert!(rendered.contains("creation-ref"), "{rendered}");
    }
}

#[cfg(unix)]
#[test]
fn error_body_policy_exposes_hook_failures_only_to_local_owner() {
    let unix = unix_client("/tmp/aoe-test.sock").expect("unix client");
    let bearer = DaemonClient::new("http://localhost", Some("token")).expect("bearer client");
    let anonymous = DaemonClient::new("http://localhost", None).expect("anonymous client");

    assert!(unix.should_read_error_body(Some(ApiErrorCode::CreateHookFailed)));
    assert!(!unix.should_read_error_body(None));
    assert!(!bearer.should_read_error_body(Some(ApiErrorCode::CreateHookFailed)));
    assert!(anonymous.should_read_error_body(None));
}
#[test]
fn mutation_receipt_requires_an_unambiguous_current_cursor() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(RUNTIME_EPOCH_HEADER, HeaderValue::from_static("current"));
    headers.insert(RUNTIME_REVISION_HEADER, HeaderValue::from_static("7"));
    assert_eq!(
        mutation_cursor(&headers, "current").unwrap(),
        RuntimeCursor {
            epoch: "current".into(),
            revision: 7
        }
    );
    for (name, values) in [
        (RUNTIME_EPOCH_HEADER, &["previous"][..]),
        (RUNTIME_EPOCH_HEADER, &["current", "current"][..]),
        (RUNTIME_REVISION_HEADER, &["7", "7"][..]),
        (RUNTIME_REVISION_HEADER, &["0"][..]),
        (RUNTIME_REVISION_HEADER, &[][..]),
    ] {
        let original = headers.remove(name).unwrap();
        for value in values {
            headers.append(name, HeaderValue::from_static(value));
        }
        assert!(matches!(
            mutation_cursor(&headers, "current"),
            Err(DaemonClientError::InvalidMutationReceipt)
        ));
        headers.remove(name);
        headers.insert(name, original);
    }
}
#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    epoch: Option<String>,
    body: Vec<u8>,
}

async fn next_request(rx: tokio::sync::oneshot::Receiver<CapturedRequest>) -> CapturedRequest {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("daemon request arrives")
        .expect("request capture")
}

#[tokio::test]
async fn stop_mutation_posts_to_stop_route_with_epoch_header() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/stop",
        200,
        vec![
            (RUNTIME_EPOCH_HEADER, "epoch-1"),
            (RUNTIME_REVISION_HEADER, "3"),
        ],
        "",
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, None).expect("test client");
    let cursor = client
        .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
        .await
        .expect("stop mutation");
    assert_eq!(
        cursor,
        RuntimeCursor {
            epoch: "epoch-1".into(),
            revision: 3,
        }
    );
    let captured = next_request(rx).await;
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/api/sessions/sess-1/stop");
    assert_eq!(captured.epoch.as_deref(), Some("epoch-1"));
    assert!(captured.body.is_empty(), "stop sends no body");
}

#[tokio::test]
async fn empty_trash_posts_profile_to_the_sessions_route() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/empty-trash",
        200,
        vec![
            (RUNTIME_EPOCH_HEADER, "epoch-1"),
            (RUNTIME_REVISION_HEADER, "3"),
        ],
        r#"{"sessions":[],"failures":[]}"#,
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, None).expect("test client");
    let receipt = client
        .empty_trash(
            &EmptyTrashBody {
                profile: "work".into(),
            },
            "epoch-1",
        )
        .await
        .expect("empty trash");
    assert!(receipt.outcome.sessions.is_empty());
    let captured = next_request(rx).await;
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/api/sessions/empty-trash");
    assert_eq!(captured.epoch.as_deref(), Some("epoch-1"));
    let body: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(body["profile"], "work");
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_lifecycle_waits_past_the_default_rpc_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for operation in ["ensure", "start", "restart", "container-terminal", "tools"] {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (arrived, observed) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 8192];
            let mut used = 0;
            while !bytes[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut bytes[used..]).await.unwrap();
                assert!(read > 0);
                used += read;
            }
            arrived.send(()).unwrap();
            released.await.unwrap();
            let body = if operation == "restart" {
                r#"{"outcome":{"lifecycle_generation":2,"profile":"work","target":null}}"#
            } else {
                r#"{"tmux_session":"ready","status":"alive","lifecycle_generation":2,"profile":"work"}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{}: epoch-1\r\n{}: 17\r\n\r\n{body}",
                body.len(), RUNTIME_EPOCH_HEADER, RUNTIME_REVISION_HEADER,
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = unix_client(&socket).unwrap();
        let pending = tokio::spawn(async move {
            let body = StartSessionBody::default();
            match operation {
                "ensure" => client
                    .ensure_agent("session", &body, "epoch-1")
                    .await
                    .map(|receipt| receipt.cursor),
                "start" => {
                    client
                        .mutate_session("session", &SessionMutation::Start(body), "epoch-1")
                        .await
                }
                "restart" => client
                    .restart_session("session", &RestartSessionBody::default(), "epoch-1")
                    .await
                    .map(|receipt| receipt.cursor),
                "container-terminal" => client
                    .ensure_container_terminal("session", 0, &body, "epoch-1")
                    .await
                    .map(|receipt| receipt.cursor),
                "tools" => client
                    .ensure_tool(
                        "session",
                        &EnsureToolBody {
                            tool_name: "shell".into(),
                            size: None,
                            adoption: None,
                        },
                        "epoch-1",
                    )
                    .await
                    .map(|receipt| receipt.cursor),
                _ => unreachable!(),
            }
        });
        observed.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(DEFAULT_TIMEOUT + Duration::from_secs(1)).await;
        tokio::time::resume();
        release.send(()).unwrap();
        let receipt = pending
            .await
            .unwrap()
            .expect("slow preparation retains its receipt");
        assert_eq!(
            receipt,
            RuntimeCursor {
                epoch: "epoch-1".into(),
                revision: 17
            },
            "{operation}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn start_mutation_posts_json_body_to_start_route() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/start",
        200,
        vec![
            (RUNTIME_EPOCH_HEADER, "epoch-7"),
            (RUNTIME_REVISION_HEADER, "11"),
        ],
        "",
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, None).expect("test client");
    let cursor = client
        .mutate_session(
            "sess-9",
            &SessionMutation::Start(StartSessionBody::default()),
            "epoch-7",
        )
        .await
        .expect("start mutation");
    assert_eq!(
        cursor,
        RuntimeCursor {
            epoch: "epoch-7".into(),
            revision: 11,
        }
    );
    let captured = next_request(rx).await;
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/api/sessions/sess-9/start");
    assert_eq!(captured.epoch.as_deref(), Some("epoch-7"));
    let body: serde_json::Value =
        serde_json::from_slice(&captured.body).expect("start sends a JSON body");
    assert_eq!(body, serde_json::json!({}));
}

#[tokio::test]
async fn restart_request_encodes_the_session_id_as_one_path_segment() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/restart",
        200,
        vec![
            (RUNTIME_EPOCH_HEADER, "epoch-1"),
            (RUNTIME_REVISION_HEADER, "2"),
        ],
        r#"{"id":"sess/a?b","lifecycle_generation":4,"profile":"root-profile","outcome":{"lifecycle_generation":5,"profile":"target-profile","target":{"tmux_session":"prepared-pane","status":"restarted","lifecycle_generation":5,"profile":"target-profile"}}}"#,
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, None).unwrap();
    let receipt = client
        .restart_session("sess/a?b", &RestartSessionBody::default(), "epoch-1")
        .await
        .unwrap();
    assert_eq!(receipt.cursor.epoch, "epoch-1");
    assert_eq!(receipt.cursor.revision, 2);
    assert_eq!(receipt.outcome.lifecycle_generation, 5);
    assert_eq!(receipt.outcome.profile, "target-profile");
    let target = receipt.outcome.target.unwrap();
    assert_eq!(target.tmux_session, "prepared-pane");
    assert_eq!(target.lifecycle_generation, 5);
    assert_eq!(target.profile, "target-profile");
    let captured = next_request(rx).await;
    assert_eq!(captured.path, "/api/sessions/sess%2Fa%3Fb/restart");
}
#[tokio::test]
async fn conflict_lifecycle_locked_maps_without_reading_body() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/stop",
        409,
        vec![(ERROR_CODE_HEADER, "lifecycle_locked")],
        r#"{"error":"lifecycle_busy","message":"Session lifecycle is busy"}"#,
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, None).expect("test client");
    let error = client
        .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
        .await
        .expect_err("conflict must fail");
    assert!(
        matches!(
            error,
            DaemonClientError::Status {
                status,
                code: Some(ApiErrorCode::LifecycleLocked),
                ref body,
                truncated: false,
            } if status == StatusCode::CONFLICT && body.is_empty()
        ),
        "409 lifecycle_locked must map from status plus header with no body read, got: {error:?}"
    );
    assert!(
        error.to_string().contains("busy with another operation"),
        "TUI-facing text must say the session is busy, got: {error}"
    );
    let captured = next_request(rx).await;
    assert_eq!(captured.path, "/api/sessions/sess-1/stop");
}

#[tokio::test]
async fn conflict_lifecycle_superseded_stays_distinct_from_busy() {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/stop",
        409,
        vec![(ERROR_CODE_HEADER, "lifecycle_superseded")],
        r#"{"error":"lifecycle_superseded","message":"changed"}"#,
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, None).expect("test client");
    let error = client
        .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
        .await
        .expect_err("conflict must fail");
    assert!(matches!(
        error,
        DaemonClientError::Status {
            code: Some(ApiErrorCode::LifecycleSuperseded),
            ref body,
            ..
        } if body.is_empty()
    ));
    let text = error.to_string();
    assert!(text.contains("changed after it was selected"), "{text}");
    assert!(!text.contains("nothing changed"), "{text}");
}

#[tokio::test]
async fn unauthorized_maps_by_status_without_body_or_token() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/stop",
        401,
        Vec::new(),
        "secret-body",
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, Some("secret-token")).expect("test client");
    let error = client
        .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
        .await
        .expect_err("unauthorized must fail");
    assert!(
        matches!(
            error,
            DaemonClientError::Status {
                status,
                code: None,
                ref body,
                ..
            } if status == StatusCode::UNAUTHORIZED && body.is_empty()
        ),
        "401 must be identifiable by status alone with no retained body, got: {error:?}"
    );
    assert!(
        !format!("{client:?}").contains("secret-token"),
        "client debug must not leak the bearer token"
    );
    assert!(
        !format!("{error}").contains("secret"),
        "error display must not reflect the response body"
    );
    let captured = next_request(rx).await;
    assert_eq!(captured.path, "/api/sessions/sess-1/stop");
}

#[test]
fn purge_waits_past_the_default_timeout_for_hooks() {
    let client = DaemonClient::new("http://127.0.0.1:1", None).unwrap();
    let request = client
        .purge_request("sess-1", &DeleteSessionBody::default())
        .unwrap()
        .build()
        .unwrap();
    let timeout = request.timeout().copied().unwrap();
    assert_eq!(timeout, PURGE_DEADLINE + DEFAULT_TIMEOUT);
    assert!(timeout > DEFAULT_TIMEOUT);
}

#[tokio::test]
async fn committed_mutation_errors_require_finite_code_and_matching_cursor() {
    for (status, code, epoch, revision, committed, invalid) in [
        (500, "start_failed", "epoch-1", "3", true, false),
        (409, "resume_failed", "epoch-1", "3", true, false),
        (409, "purge_failed", "epoch-1", "3", true, false),
        (409, "managed_resume_refused", "epoch-1", "3", true, false),
        (
            409,
            "sandbox_migration_deferred",
            "epoch-1",
            "3",
            true,
            false,
        ),
        (500, "sandbox_migration_failed", "epoch-1", "3", true, false),
        (
            409,
            "sandbox_migration_deferred",
            "absent",
            "absent",
            false,
            true,
        ),
        (
            500,
            "sandbox_migration_failed",
            "absent",
            "absent",
            false,
            true,
        ),
        (
            409,
            "sandbox_migration_deferred",
            "foreign",
            "3",
            false,
            true,
        ),
        (500, "sandbox_migration_failed", "epoch-1", "0", false, true),
        (409, "managed_resume_refused", "foreign", "3", false, true),
        (409, "managed_resume_refused", "epoch-1", "0", false, true),
        (
            409,
            "managed_resume_refused",
            "absent",
            "absent",
            false,
            false,
        ),
        (
            409,
            "managed_resume_refused",
            "epoch-1",
            "absent",
            false,
            true,
        ),
        (
            409,
            "managed_resume_refused",
            "epoch-1",
            "3, 4",
            false,
            true,
        ),
        (500, "managed_resume_refused", "epoch-1", "3", false, false),
        (
            409,
            "managed_resume_refused, resume_failed",
            "epoch-1",
            "3",
            false,
            false,
        ),
        (409, "MANAGED_RESUME_REFUSED", "epoch-1", "3", false, false),
        (409, "purge_failed", "foreign", "3", false, true),
        (409, "purge_failed", "epoch-1", "0", false, true),
        (409, "purge_failed", "absent", "absent", false, true),
        (500, "purge_failed", "epoch-1", "3", false, false),
        (409, "resume_failed", "foreign", "3", false, true),
        (409, "resume_failed", "epoch-1", "0", false, true),
        (409, "resume_failed", "epoch-1", "3, 4", false, true),
        (409, "unknown", "epoch-1", "3", false, false),
        (403, "resume_failed", "epoch-1", "3", false, false),
    ] {
        let purge = code == "purge_failed";
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut headers = vec![(ERROR_CODE_HEADER, code)];
        if epoch != "absent" {
            headers.push((RUNTIME_EPOCH_HEADER, epoch));
        }
        if revision != "absent" {
            headers.push((RUNTIME_REVISION_HEADER, revision));
        }
        let base = serve_mutation_once(
            if purge {
                "/api/sessions/{id}"
            } else {
                "/api/sessions/{id}/start"
            },
            status,
            headers,
            "secret-body",
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, Some("secret-token")).unwrap();
        let error = if purge {
            client
                .purge_session("sess-1", &DeleteSessionBody::default(), "epoch-1")
                .await
                .unwrap_err()
        } else {
            client
                .mutate_session(
                    "sess-1",
                    &SessionMutation::Start(StartSessionBody::default()),
                    "epoch-1",
                )
                .await
                .unwrap_err()
        };
        assert!(!error.to_string().contains("secret"));
        if committed {
            assert!(
                matches!(error, DaemonClientError::Committed { cursor, .. } if cursor.epoch == "epoch-1" && cursor.revision == 3)
            );
        } else if invalid {
            assert!(matches!(error, DaemonClientError::InvalidMutationReceipt));
        } else {
            assert!(matches!(error, DaemonClientError::Status { .. }));
        }
        assert_eq!(next_request(rx).await.epoch.as_deref(), Some("epoch-1"));
    }
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let base = serve_mutation_once(
        "/api/sessions/{id}/start",
        409,
        vec![
            (ERROR_CODE_HEADER, "resume_failed"),
            (RUNTIME_EPOCH_HEADER, "epoch-1"),
            (RUNTIME_REVISION_HEADER, "3"),
        ],
        "secret-body",
        tx,
    )
    .await;
    let client = DaemonClient::new(&base, Some("secret-token")).unwrap();
    let error = client
        .request_response(
            client
                .http
                .post(format!("{base}/api/sessions/sess-1/start")),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, DaemonClientError::Status { .. }),
        "a non-mutation request cannot adopt an error ACK"
    );
}

#[test]
fn smart_rename_errors_do_not_reflect_agent_output() {
    for code in [
        ApiErrorCode::SmartRenameSkipped,
        ApiErrorCode::SmartRenameFailed,
        ApiErrorCode::SmartRenameTimeout,
    ] {
        let error = DaemonClientError::Status {
            status: code.status(),
            code: Some(code),
            body: "secret-agent-output".into(),
            truncated: false,
        };
        assert!(!error.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn unreachable_daemon_surfaces_transport_error() {
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        listener.local_addr().expect("ephemeral addr").port()
    };
    let client = DaemonClient::new(&format!("http://127.0.0.1:{port}"), None).expect("test client");
    let error = client
        .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
        .await
        .expect_err("unreachable daemon must fail");
    assert!(
        matches!(error, DaemonClientError::Transport),
        "unreachable daemon must surface a transport error with no local write, got: {error:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cli_creation_hook_details_stay_available_only_to_the_unix_owner() {
    for (unix, authenticated) in [(true, false), (false, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let handler = |req: Request<Incoming>| async move {
            assert_eq!(req.uri().path(), "/api/sessions");
            assert_eq!(req.method(), reqwest::Method::POST);
            req.into_body().collect().await.unwrap();
            reply(
                400,
                ApiErrorCode::CreateHookFailed.header(),
                "on_create hook failed: LOCAL_HOOK_SENTINEL",
            )
        };
        let (client, server) = if unix {
            let path = root.path().join("api.sock");
            (
                unix_client(&path).unwrap(),
                serve_unix(&path, handler).await,
            )
        } else {
            let (url, server) = serve_tcp(handler).await;
            (
                DaemonClient::new(&url, authenticated.then_some("credential")).unwrap(),
                server,
            )
        };
        let body = serde_json::from_value(serde_json::json!({
            "path": "/tmp", "tool": "claude", "cli": {},
        }))
        .unwrap();
        let error = client
            .create_session(&body, "epoch", PromptOrigin::Unknown)
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            DaemonClientError::Status {
                status: StatusCode::BAD_REQUEST,
                code: Some(ApiErrorCode::CreateHookFailed),
                ..
            }
        ));
        assert_eq!(error.to_string().contains("LOCAL_HOOK_SENTINEL"), unix);
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn empty_trash_waits_for_a_slow_batch_within_its_own_deadline() {
    for timeout in [Duration::from_millis(50), Duration::from_secs(10)] {
        let (base, server) = serve_tcp(|req| async move {
            assert_eq!(req.uri().path(), "/api/sessions/empty-trash");
            req.into_body().collect().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            reply(
                200,
                [
                    (RUNTIME_EPOCH_HEADER, "epoch-1"),
                    (RUNTIME_REVISION_HEADER, "3"),
                ],
                r#"{"sessions":[],"failures":[]}"#,
            )
        })
        .await;
        let client = DaemonClient::new(&base, None).unwrap();
        let body = EmptyTrashBody {
            profile: "work".into(),
        };
        let result = client.empty_trash_within(&body, "epoch-1", timeout).await;
        if timeout < Duration::from_secs(1) {
            assert!(
                matches!(result, Err(DaemonClientError::Timeout)),
                "{result:?}"
            );
            server.abort();
            let _ = server.await;
        } else {
            assert_eq!(result.unwrap().cursor.revision, 3);
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn smart_rename_accepts_completed_and_supervised_receipts() {
    for status in [200, 202] {
        let (base, server) = serve_tcp(move |req| async move {
            assert_eq!(req.uri().path(), "/api/sessions/sess-1/smart-rename");
            assert_eq!(req.method(), reqwest::Method::POST);
            assert_eq!(req.headers()[RUNTIME_EPOCH_HEADER], "epoch-1");
            reply(
                status,
                [
                    (RUNTIME_EPOCH_HEADER, "epoch-1"),
                    (RUNTIME_REVISION_HEADER, "3"),
                ],
                r#"{"id":"sess-1","title":"named"}"#,
            )
        })
        .await;
        let client = DaemonClient::new(&base, None).unwrap();
        let receipt = client
            .smart_rename_session("sess-1", "epoch-1")
            .await
            .unwrap();
        assert_eq!(receipt.cursor.epoch, "epoch-1");
        assert_eq!(receipt.cursor.revision, 3);
        assert_eq!(receipt.outcome.title, "named");
        server.await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn managed_resume_refusal_does_not_reflect_private_bodies() {
    for (unix, authenticated, cause) in [
        (false, false, "wrapper_contract_required"),
        (false, true, "wrapper_contract_required"),
        (true, false, "wrapper_contract_required"),
        (false, false, "pi_transcript_binding_required"),
        (false, true, "pi_transcript_binding_required"),
        (true, false, "pi_transcript_binding_required"),
        (false, false, "omp_transcript_binding_required"),
        (false, true, "omp_transcript_binding_required"),
        (true, false, "omp_transcript_binding_required"),
        (false, false, "secret-token"),
    ] {
        let expected = match cause {
            "wrapper_contract_required" => ManagedResumeCause::WrapperContractRequired,
            "pi_transcript_binding_required" => ManagedResumeCause::PiTranscriptBindingRequired,
            "omp_transcript_binding_required" => ManagedResumeCause::OmpTranscriptBindingRequired,
            _ => ManagedResumeCause::ExecutionContextUnattested,
        };
        let handler = move |req: Request<Incoming>| async move {
            assert_eq!(req.uri().path(), "/api/sessions/sess-1/start");
            req.into_body().collect().await.unwrap();
            reply(
                409,
                [
                    (ERROR_CODE_HEADER, "managed_resume_refused"),
                    (MANAGED_RESUME_CAUSE_HEADER, cause),
                    (RUNTIME_EPOCH_HEADER, "epoch-1"),
                    (RUNTIME_REVISION_HEADER, "3"),
                ],
                "private-path=/secret/store native-id=secret-id credential=secret-token",
            )
        };
        for committed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("api.sock");
            let (client, server, url) = if unix {
                (
                    unix_client(&socket).unwrap(),
                    serve_unix(&socket, handler).await,
                    "http://localhost".into(),
                )
            } else {
                let (url, server) = serve_tcp(handler).await;
                let client =
                    DaemonClient::new(&url, authenticated.then_some("secret-token")).unwrap();
                (client, server, url)
            };
            let error = if committed {
                client
                    .mutate_session(
                        "sess-1",
                        &SessionMutation::Start(StartSessionBody::default()),
                        "epoch-1",
                    )
                    .await
                    .unwrap_err()
            } else {
                client
                    .request_response(client.http.post(format!("{url}/api/sessions/sess-1/start")))
                    .await
                    .unwrap_err()
            };
            let message = error.to_string();
            assert_eq!(message, expected.message());
            assert!(!format!("{error:?}").contains("secret"));
            assert!(!message.contains("secret"), "{message}");
            if expected == ManagedResumeCause::WrapperContractRequired {
                assert!(message.contains("session.agent_execution_as"), "{message}");
                assert!(message.contains("session.agent_config_dir"), "{message}");
            }
            if matches!(
                expected,
                ManagedResumeCause::PiTranscriptBindingRequired
                    | ManagedResumeCause::OmpTranscriptBindingRequired
            ) {
                let agent = if expected == ManagedResumeCause::PiTranscriptBindingRequired {
                    "Pi"
                } else {
                    "OMP"
                };
                assert!(
                    message
                        .split(|ch: char| !ch.is_ascii_alphanumeric())
                        .any(|token| token == agent),
                    "{message}"
                );
            }
            if committed {
                assert!(
                    matches!(error, DaemonClientError::Committed { cursor, code }
                    if cursor.revision == 3 && code == ApiErrorCode::ManagedResumeRefused(expected))
                );
            } else {
                assert!(
                    matches!(error, DaemonClientError::Status { code: Some(code), body, .. }
                    if code == ApiErrorCode::ManagedResumeRefused(expected) && body.is_empty())
                );
            }
            server.await.unwrap();
        }
    }
}
