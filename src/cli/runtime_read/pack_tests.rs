//! The client's half of the read-only contract, driven by the frozen
//! Contract Pack: the recorded transcripts must decode through the real
//! decoders, the frozen goldens must be what the real renderer produces, and
//! the upgrade failures must classify exactly as the phase/code matrix says.
//!
//! One code in the phase/code table is a refusal no committed case can
//! produce. `peer_identity` means a peer was reached and is not the publisher,
//! and every refusal case here was recorded from a transcript that was not: a
//! disagreeing one would have had to be recorded from a publisher of another
//! user, which is not a thing this pack can record. It stays in the table
//! because the table may not name a code the client cannot emit and three
//! sites emit it. The disagreement branch is driven directly instead, in
//! `dto`'s own tests, which is where the rule lives. What is still without a
//! test is the same code at two other sites, and both need a peer this process
//! cannot be: `uds`'s `SO_PEERCRED` walk, and the Hello-against-admitted-
//! identity comparison in `mod`.

use std::net::SocketAddr;

use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::dto::{
    parse_hello, parse_snapshot, validate_cross_message, validate_hello, validate_snapshot,
};
use super::endpoint::ReadRequestSource;
use super::pack::{self, VerifiedCase};
use super::render;
use super::{map_upgrade_error, Cli, ReadOutcome, ScopedCommand, WsError};
use crate::cli::list::{ListArgs, StateFilter};
use crate::cli::status::StatusArgs;
use crate::server::runtime_ws::PROTOCOL_VERSION;

fn pack() -> pack::VerifiedPack {
    pack::verify(&pack::pack_root()).expect("the committed Contract Pack verifies")
}

/// The unmasked application frames the server sent in a transcript, in order.
fn application_frames(case: &VerifiedCase) -> Vec<Vec<u8>> {
    case.wire
        .iter()
        .filter(|record| record.is_server_to_client())
        .map(|record| record.bytes.clone())
        .filter(|payload| !payload.starts_with(b"HTTP/1."))
        // Ping/Pong/Close are control frames, not application messages.
        .filter(|payload| payload.first().is_some_and(|byte| byte & 0x0F < 0x8))
        .collect()
}

/// The UTF-8 body of one text frame, checked against the declared length.
fn text_payload(frame: &[u8]) -> String {
    assert_eq!(frame[0] & 0x0F, 0x1, "expected a text frame");
    let (length, cursor) = match frame[1] & 0x7F {
        126 => {
            let raw: [u8; 2] = frame[2..4].try_into().expect("two length bytes");
            (u16::from_be_bytes(raw) as usize, 4)
        }
        127 => {
            let raw: [u8; 8] = frame[2..10].try_into().expect("eight length bytes");
            (u64::from_be_bytes(raw) as usize, 10)
        }
        short => (short as usize, 2),
    };
    assert_eq!(
        frame.len(),
        cursor + length,
        "frame length disagrees with its bytes"
    );
    String::from_utf8(frame[cursor..].to_vec()).expect("application frames are UTF-8")
}

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
fn every_nominal_transcript_decodes_and_validates() {
    let pack = pack();
    let mut checked = 0;
    for case in &pack.cases {
        // A case with a nonzero exit is frozen precisely because it is rejected.
        if case.exit != 0 {
            continue;
        }
        let frames = application_frames(case);
        assert_eq!(
            frames.len(),
            2,
            "case {} carries two application frames",
            case.case_id
        );
        let hello = parse_hello(text_payload(&frames[0]).as_bytes())
            .unwrap_or_else(|error| panic!("case {} Hello: {error:?}", case.case_id));
        let snapshot = parse_snapshot(text_payload(&frames[1]).as_bytes())
            .unwrap_or_else(|_| panic!("case {} Snapshot does not decode", case.case_id));
        validate_hello(&hello)
            .unwrap_or_else(|code| panic!("case {} Hello is {code}", case.case_id));
        validate_snapshot(&snapshot)
            .unwrap_or_else(|code| panic!("case {} Snapshot is {code}", case.case_id));
        // The uds transcript is the one that declares a local owner, so it is
        // the one the local arm of the cross-message check applies to. It used
        // to be skipped for that reason, which left the arm cold; the uid is
        // the one the recorded Hello carries, read from that Hello rather than
        // written down here, so the case cannot disagree with itself.
        let local_uid = (case.transport == "uds")
            .then_some(hello.owner.uid)
            .flatten();
        validate_cross_message(&hello, &snapshot, local_uid)
            .unwrap_or_else(|code| panic!("case {} cross-message is {code}", case.case_id));
        checked += 1;
    }
    assert!(
        checked >= 2,
        "expected two nominal transcripts, got {checked}"
    );
}
#[test]
#[serial_test::parallel]
fn required_nullable_fields_reject_omission_but_accept_null() {
    let pack = pack();
    let case = pack.case("uds-list-nominal").expect("nominal case");
    let frames = application_frames(case);
    let mut hello: serde_json::Value = serde_json::from_str(&text_payload(&frames[0])).unwrap();
    let mut snapshot: serde_json::Value = serde_json::from_str(&text_payload(&frames[1])).unwrap();
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

/// The frozen schema-invalid Snapshot decodes structurally and is then rejected
/// by the DTO validator, which is what makes it a snapshot-phase failure.
#[test]
#[serial_test::parallel]
fn the_frozen_schema_invalid_snapshot_is_rejected() {
    let pack = pack();
    let case = pack
        .case("http-loopback-snapshot-schema-invalid")
        .expect("the pack ships a schema-invalid case");
    let frames = application_frames(case);
    let snapshot =
        parse_snapshot(text_payload(&frames[1]).as_bytes()).expect("the payload is a Snapshot");
    assert_eq!(
        validate_snapshot(&snapshot),
        Err("schema_invalid"),
        "a dangling default_profile must be schema_invalid"
    );
    assert_eq!(case.code.as_deref(), Some("schema_invalid"));
    assert_eq!(case.exit, 4);
    assert_eq!(case.stderr, b"daemon read: schema_invalid\n");
}

/// A Hello that announces a protocol this client does not speak is a transport
/// mismatch, checked before any other field is looked at.
#[test]
#[serial_test::parallel]
fn a_hello_from_another_protocol_version_is_a_transport_mismatch() {
    let pack = pack();
    let case = pack
        .case("uds-list-nominal")
        .expect("the pack ships a UDS case");
    let frames = application_frames(case);
    let hello = text_payload(&frames[0]);
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
    let pack = pack();
    let case = pack
        .case("uds-list-nominal")
        .expect("the pack ships a UDS case");
    let frames = application_frames(case);
    let hello = text_payload(&frames[0]);
    let broken = hello.replacen("\"namespace\"", "\"name_space\"", 1);
    assert_ne!(hello, broken, "the Hello carries a namespace member");
    assert!(matches!(
        parse_hello(broken.as_bytes()),
        Err(super::dto::HelloParseError::Schema)
    ));
}

/// The frozen golden for `aoe list` is exactly what the renderer emits for the
/// frozen Snapshot.
#[test]
#[serial_test::parallel]
fn the_frozen_list_golden_matches_the_renderer() {
    let pack = pack();
    let case = pack
        .case("uds-list-nominal")
        .expect("the pack ships a UDS case");
    let frames = application_frames(case);
    let snapshot = parse_snapshot(text_payload(&frames[1]).as_bytes()).expect("Snapshot decodes");
    let args = ListArgs {
        json: false,
        all: false,
        state: StateFilter::All,
    };
    let rendered = render::evaluate(
        &ScopedCommand::List(&args),
        &snapshot,
        &default_source(),
        None,
    )
    .expect("the list renders");
    assert_eq!(rendered.stdout.as_bytes(), case.stdout);
}

/// The frozen golden for `aoe status --json` likewise.
#[test]
#[serial_test::parallel]
fn the_frozen_status_golden_matches_the_renderer() {
    let pack = pack();
    let case = pack
        .case("http-loopback-status-nominal")
        .expect("the pack ships a loopback case");
    let frames = application_frames(case);
    let snapshot = parse_snapshot(text_payload(&frames[1]).as_bytes()).expect("Snapshot decodes");
    let args = StatusArgs {
        json: true,
        quiet: false,
        verbose: false,
    };
    let rendered = render::evaluate(
        &ScopedCommand::Status(&args),
        &snapshot,
        &default_source(),
        None,
    )
    .expect("the status renders");
    assert_eq!(rendered.stdout.as_bytes(), case.stdout);
}

/// The frozen frame counts are the static evidence the verifier checks, and
/// they only make sense alongside what each transcript actually contains.
#[test]
#[serial_test::parallel]
fn frozen_frame_counts_match_the_transcripts() {
    let pack = pack();
    let nominal = pack
        .case("uds-list-nominal")
        .expect("the pack ships a UDS case");
    assert_eq!(nominal.replay_role, "server_mode");
    assert_eq!(nominal.wire_frames, 2);
    assert_eq!(application_frames(nominal).len(), 2);

    let denied = pack
        .case("http-loopback-unauthorized")
        .expect("the pack ships a loopback case");
    assert_eq!(denied.replay_role, "client_mode");
    assert_eq!(denied.wire_frames, 0);
    assert!(application_frames(denied).is_empty());
    assert_eq!(denied.exit, 4);
    assert_eq!(denied.stderr, b"daemon read: unauthorized\n");
}

/// Every frozen argv row reaches the scoped reader, and none of them is a
/// parser error.
#[test]
#[serial_test::parallel]
fn the_frozen_argv_rows_all_reach_the_scoped_reader() {
    for case in &pack().cases {
        assert_eq!(case.parse, "ok", "case {}", case.case_id);
        let cli = Cli::try_parse_from(case.argv.clone())
            .unwrap_or_else(|error| panic!("case {} argv: {error}", case.case_id));
        assert!(
            super::classify(cli.command.as_ref()).is_some(),
            "case {} does not classify as a scoped read",
            case.case_id
        );
    }
}

/// A frozen alias selects the same command as its canonical spelling.
#[test]
#[serial_test::parallel]
fn the_frozen_aliases_select_the_same_command() {
    for case in &pack().cases {
        let cli = Cli::try_parse_from(case.argv.clone()).expect("argv parses");
        let Some(command) = super::classify(cli.command.as_ref()) else {
            // Not a read command, so this gate has nothing to say about it --
            // but a *read* command that stopped classifying is not something to
            // skip over, so the case's own command name decides.
            if case.command != "none" {
                panic!(
                    "case {} names the read command {} but it does not classify",
                    case.case_id, case.command
                );
            }
            continue;
        };
        // The canonical spelling depends on the *command*, never on which
        // spelling the case exercised. Deriving it from the alias meant an
        // alias case parsed `ls` and compared it with `ls`, so a classifier that
        // sent `ls` to the wrong command while `list` stayed right passed here.
        let canonical: &[&str] = match case.command.as_str() {
            "list" => &["aoe", "list"],
            "status" => &["aoe", "status"],
            "session-show" => &["aoe", "session", "show", "s-1"],
            "session-list-trash" => &["aoe", "session", "list-trash"],
            "group-list" => &["aoe", "group", "list"],
            "profile-bare" => &["aoe", "profile"],
            "profile-list" => &["aoe", "profile", "list"],
            "project-list" => &["aoe", "project", "list"],
            other => panic!("unknown command {other}"),
        };
        let direct = Cli::try_parse_from(canonical).expect("canonical form parses");
        let expected = super::classify(direct.command.as_ref()).expect("canonical classifies");
        assert_eq!(
            std::mem::discriminant(&command),
            std::mem::discriminant(&expected),
            "case {} alias",
            case.case_id
        );
    }
}

/// A complete record is its header plus its payload, so replay can compare the
/// exact bytes the fixture froze.
#[test]
#[serial_test::parallel]
fn a_wire_record_round_trips_through_its_header() {
    let pack = pack();
    let case = pack
        .case("http-loopback-unauthorized")
        .expect("the pack ships a loopback case");
    for record in &case.wire {
        let raw = record.raw();
        assert_eq!(&raw[..2], &[record.direction, record.role]);
        let length = u32::from_be_bytes(raw[2..6].try_into().expect("four bytes")) as usize;
        assert_eq!(raw.len(), 6 + length);
        assert_eq!(&raw[6..], record.bytes.as_slice());
    }
}

/// Serve one canned HTTP response on loopback and return what the upgrade did.
async fn upgrade_against(response: &'static [u8]) -> Result<(), WsError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address: SocketAddr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut head = [0u8; 2048];
        let _ = stream.read(&mut head).await;
        stream.write_all(response).await.expect("write response");
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

const UNAUTHORIZED: &[u8] =
    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const FORBIDDEN: &[u8] =
    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const REDIRECT: &[u8] =
    b"HTTP/1.1 302 Found\r\nLocation: http://example.test/\r\nContent-Length: 0\r\n\r\n";
const SERVER_ERROR: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
/// A 101 whose `Sec-WebSocket-Accept` is not the RFC 6455 digest of the key the
/// client sent.
const WRONG_ACCEPT: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: AAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n\r\n";

#[tokio::test]
#[serial_test::parallel]
async fn a_401_or_403_upgrade_is_unauthorized_at_exit_four() {
    for response in [UNAUTHORIZED, FORBIDDEN] {
        let error = upgrade_against(response)
            .await
            .expect_err("a denied upgrade must not connect");
        let failure = map_upgrade_error(error);
        assert_eq!(failure.code(), "unauthorized");
        let outcome = ReadOutcome::from(failure);
        assert_eq!(outcome.stdout, None);
        assert_eq!(
            outcome.stderr.as_deref(),
            Some("daemon read: unauthorized\n")
        );
        assert_eq!(outcome.exit, 4);
    }
}

/// A redirect and a 500 are the same class and the opposite of an absent
/// endpoint: the peer answered and would not serve us. Answering from the local
/// store here would print this machine's sessions as the remote's, so both
/// refuse, under a name that says which happened.
#[tokio::test]
#[serial_test::parallel]
async fn a_redirect_or_error_upgrade_is_a_server_error() {
    for response in [REDIRECT, SERVER_ERROR] {
        let error = upgrade_against(response)
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
    let error = upgrade_against(WRONG_ACCEPT)
        .await
        .expect_err("a forged accept must not connect");
    assert_eq!(map_upgrade_error(error).code(), "unavailable");
}
