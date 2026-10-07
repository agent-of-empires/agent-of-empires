//! Recorded application exchanges and upgrade refusals at the client boundary.

use super::dto::{parse_hello, parse_snapshot};
use super::endpoint::ReadRequestSource;
use super::pack;
use super::{map_upgrade_error, Cli, ReadOutcome, WsError};
use crate::server::runtime_ws::PROTOCOL_VERSION;
use clap::Parser;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// A read source with no explicit profile and no environment, so the Snapshot
/// default profile is the only selector.
fn default_source() -> ReadRequestSource {
    ReadRequestSource {
        explicit_url: None,
        env_url: None,
        token: None,
        explicit_profile: None,
        env_profile: None,
    }
}

#[test]
#[serial_test::parallel]
fn required_nullable_fields_reject_omission_but_accept_null() {
    let frames = pack::application_frames(pack::fixture("uds-list-nominal").wire);
    let mut hello: serde_json::Value = serde_json::from_slice(frames[0].as_bytes()).unwrap();
    let mut snapshot: serde_json::Value = serde_json::from_slice(frames[1].as_bytes()).unwrap();
    snapshot["data"]["sessions"][0]["worktree"] = serde_json::json!({
        "branch": "main", "main_repo_path": "/repo", "managed_by_aoe": false, "base_branch": null
    });
    let unobserved =
        serde_json::json!({ "kind": "unobserved", "revision": 0, "observed_at": null });
    hello["data"]["status_freshness"] = unobserved.clone();
    snapshot["data"]["status_freshness"] = unobserved;
    for (frame, paths) in [
        (
            &hello,
            vec!["/data/owner/uid", "/data/status_freshness/observed_at"],
        ),
        (
            &snapshot,
            vec![
                "/data/default_profile",
                "/data/resolved_default_profile",
                "/data/status_freshness/observed_at",
                "/data/profiles/0/projects/0/default_base_branch",
                "/data/sessions/0/last_accessed_at",
                "/data/sessions/0/idle_entered_at",
                "/data/sessions/0/last_error",
                "/data/sessions/0/archived_at",
                "/data/sessions/0/trashed_at",
                "/data/sessions/0/active_snoozed_until",
                "/data/sessions/0/pinned_at",
                "/data/sessions/0/agent_session_id",
                "/data/sessions/0/parent_session_id",
                "/data/sessions/0/worktree",
                "/data/sessions/0/worktree/base_branch",
            ],
        ),
    ] {
        for path in paths {
            let (parent, field) = path.rsplit_once('/').unwrap();
            let mut value = frame.clone();
            value
                .pointer_mut(path)
                .unwrap_or_else(|| panic!("fixture lacks {path}"));
            let decodes = |value: &serde_json::Value| {
                let bytes = serde_json::to_vec(value).unwrap();
                if value["kind"] == "hello" {
                    parse_hello(&bytes).is_ok()
                } else {
                    parse_snapshot(&bytes).is_ok()
                }
            };
            value
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(!decodes(&value), "omission accepted: {path}");
            value.pointer_mut(parent).unwrap()[field] = serde_json::Value::Null;
            assert!(decodes(&value), "explicit null refused: {path}");
        }
    }
    for value in [
        serde_json::json!({"kind":"unobserved","revision":0,"observed_at":null}),
        serde_json::json!({"kind":"unavailable","revision":null,"observed_at":null}),
    ] {
        for field in ["revision", "observed_at"] {
            let mut omitted = value.clone();
            omitted.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<super::dto::StatusFreshness>(omitted).is_err());
        }
        assert!(matches!(
            serde_json::from_value::<super::dto::StatusFreshness>(value).unwrap(),
            super::dto::StatusFreshness::Unobserved {
                observed_at: None,
                ..
            } | super::dto::StatusFreshness::Unavailable {
                revision: None,
                observed_at: None
            }
        ));
    }
}

/// A Hello that announces a protocol this client does not speak is a transport
/// mismatch, checked before any other field is looked at.
#[test]
#[serial_test::parallel]
fn a_hello_from_another_protocol_version_is_a_transport_mismatch() {
    let frames = pack::application_frames(pack::fixture("uds-list-nominal").wire);
    let hello = frames[0].as_str();
    let emitted = PROTOCOL_VERSION;
    let downgraded = hello.replace(
        &format!("\"protocol_version\":{emitted}"),
        &format!("\"protocol_version\":{}", emitted - 1),
    );
    assert_ne!(hello, downgraded, "the Hello states its version");
    assert!(matches!(
        parse_hello(downgraded.as_bytes()),
        Err(super::dto::HelloParseError::ProtocolVersion)
    ));
}

/// A Hello whose DTO shape is wrong is a handshake failure, not a version
/// mismatch.
#[test]
#[serial_test::parallel]
fn a_malformed_hello_dto_is_a_handshake_schema_failure() {
    let frames = pack::application_frames(pack::fixture("uds-list-nominal").wire);
    let hello = frames[0].as_str();
    let broken = hello.replacen("\"namespace\"", "\"name_space\"", 1);
    assert_ne!(hello, broken, "the Hello carries a namespace member");
    assert!(matches!(
        parse_hello(broken.as_bytes()),
        Err(super::dto::HelloParseError::Schema)
    ));
}

/// Serve one canned HTTP response on loopback and return what the upgrade did.
async fn upgrade_against(response: Vec<u8>) -> Result<(), WsError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address: SocketAddr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut head = [0u8; 2048];
        let _ = stream.read(&mut head).await;
        stream.write_all(&response).await.expect("write response");
        stream.flush().await.ok();
        let _ = stream.read(&mut head).await;
    });
    let request = format!("ws://{address}/api/runtime/ws")
        .into_client_request()
        .expect("request builds");
    let outcome = tokio_tungstenite::connect_async(request).await;
    server.abort();
    outcome.map(|_| ())
}

const REDIRECT: &[u8] =
    b"HTTP/1.1 302 Found\r\nLocation: http://example.test/\r\nContent-Length: 0\r\n\r\n";
const SERVER_ERROR: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
/// A 101 whose `Sec-WebSocket-Accept` is not the RFC 6455 digest of the key the
/// client sent.
const WRONG_ACCEPT: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: AAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n\r\n";

/// A redirect and a 500 are the same class and the opposite of an absent
/// endpoint: the peer answered and would not serve us. Answering from the local
/// store here would print this machine's sessions as the remote's, so both
/// refuse, under a name that says which happened.
#[tokio::test]
#[serial_test::parallel]
async fn a_redirect_or_error_upgrade_is_a_server_error() {
    for response in [REDIRECT, SERVER_ERROR] {
        let error = upgrade_against(response.to_vec())
            .await
            .expect_err("a non-101 upgrade must not connect");
        let failure = map_upgrade_error(error);
        assert_eq!(failure.code(), "server_error");
        let outcome = ReadOutcome::from(failure);
        assert_eq!(outcome.stdout, None);
        assert_eq!(
            outcome.stderr.as_deref(),
            Some("daemon read: server_error\n")
        );
        assert_eq!(outcome.exit, 4);
    }
}

#[tokio::test]
#[serial_test::parallel]
async fn an_upgrade_with_the_wrong_accept_is_unavailable() {
    let error = upgrade_against(WRONG_ACCEPT.to_vec())
        .await
        .expect_err("a forged accept must not connect");
    assert_eq!(map_upgrade_error(error).code(), "unavailable");
}

#[tokio::test]
#[serial_test::parallel]
async fn all_recorded_exchanges_preserve_the_cli_answers() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{protocol::Role, Message};
    let cases: &[(&str, &[&str], i32, Option<&str>)] = &[
        (
            "http-loopback-forbidden",
            &["aoe", "ls"],
            4,
            Some("unauthorized"),
        ),
        (
            "http-loopback-snapshot-schema-invalid",
            &["aoe", "list", "--json"],
            4,
            Some("schema_invalid"),
        ),
        (
            "http-loopback-status-nominal",
            &["aoe", "status", "--json"],
            0,
            None,
        ),
        (
            "http-loopback-unauthorized",
            &["aoe", "list"],
            4,
            Some("unauthorized"),
        ),
        (
            "https-unauthorized",
            &["aoe", "group", "ls"],
            4,
            Some("unauthorized"),
        ),
        ("uds-list-nominal", &["aoe", "list"], 0, None),
        (
            "http-loopback-default-missing",
            &["aoe", "status"],
            1,
            Some("default_missing"),
        ),
    ];
    for &(name, argv, exit, code) in cases {
        #[cfg(not(target_os = "linux"))]
        if name == "uds-list-nominal" {
            continue;
        }
        let fixture = pack::fixture(name);
        let cli = Cli::try_parse_from(argv).expect("recorded argv");
        let command = super::classify(cli.command.as_ref()).expect("scoped read");
        let outcome = if code == Some("unauthorized") {
            let response = fixture
                .wire
                .into_iter()
                .find(|record| record.direction == 2 && record.bytes.starts_with(b"HTTP/"))
                .expect("recorded HTTP refusal")
                .bytes;
            let failure = map_upgrade_error(
                upgrade_against(response)
                    .await
                    .expect_err("recorded denied upgrade"),
            );
            assert_eq!(failure.code(), "unauthorized", "{name}");
            ReadOutcome::from(failure)
        } else {
            let frames = pack::application_frames(fixture.wire);
            let mut expected = super::ExpectedPeer::Remote;
            #[cfg(target_os = "linux")]
            if name == "uds-list-nominal" {
                let hello = parse_hello(frames[0].as_bytes()).unwrap();
                expected = super::ExpectedPeer::Local(super::uds::UdsIdentity {
                    namespace: hello.namespace,
                    prebind_instance_id: hello.prebind_instance_id,
                    runtime_instance_id: hello.runtime_instance_id,
                    runtime_epoch: hello.runtime_epoch,
                    owner_uid: 501,
                });
            }
            let (client, peer) = tokio::io::duplex(64 * 1024);
            let server = tokio::spawn(async move {
                let mut socket =
                    tokio_tungstenite::WebSocketStream::from_raw_socket(peer, Role::Server, None)
                        .await;
                for frame in frames {
                    socket.send(Message::Text(frame)).await.unwrap();
                }
                socket.close(None).await.unwrap();
                while let Some(message) = socket.next().await {
                    if matches!(message, Ok(Message::Close(_))) {
                        break;
                    }
                    message.unwrap();
                }
            });
            let socket =
                tokio_tungstenite::WebSocketStream::from_raw_socket(client, Role::Client, None)
                    .await;
            let answer = super::exchange_stream(
                socket,
                tokio::time::Instant::now() + super::CONNECTION_BUDGET,
                expected,
                None,
                command,
                &default_source(),
            )
            .await;
            let outcome = match answer {
                Ok(projection) => ReadOutcome {
                    stdout: Some(projection.stdout),
                    stderr: None,
                    exit: 0,
                },
                Err(failure) => {
                    assert_eq!(Some(failure.code()), code, "{name}");
                    ReadOutcome::from(failure)
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            outcome
        };
        assert_eq!(outcome.exit, exit, "{name}");
        assert_eq!(
            outcome.stdout.as_deref().unwrap_or("").as_bytes(),
            fixture.stdout,
            "{name}: stdout"
        );
        assert_eq!(
            outcome.stderr.as_deref().unwrap_or("").as_bytes(),
            fixture.stderr,
            "{name}: stderr"
        );
    }
}
