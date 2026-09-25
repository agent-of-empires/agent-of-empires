//! The client's half of the read-only contract, driven by the frozen
//! Contract Pack: the recorded transcripts must decode through the real
//! decoders, the frozen goldens must be what the real renderer produces, and
//! the upgrade failures must classify exactly as the phase/code matrix says.

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
        if case.transport != "uds" {
            validate_cross_message(&hello, &snapshot, None)
                .unwrap_or_else(|code| panic!("case {} cross-message is {code}", case.case_id));
        }
        checked += 1;
    }
    assert!(
        checked >= 2,
        "expected two nominal transcripts, got {checked}"
    );
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
    let downgraded = hello.replacen("\"protocol_version\":2", "\"protocol_version\":1", 1);
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
    assert_eq!(rendered.as_bytes(), case.stdout);
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
    assert_eq!(rendered.as_bytes(), case.stdout);
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

/// The identifier-required pre-dispatch result wins over every runtime error and
/// exits 2 with its own message, not the uniform `daemon read:` line.
#[tokio::test]
#[serial_test::parallel]
async fn a_show_without_an_identifier_exits_two_before_transport() {
    let cli = Cli::try_parse_from(["aoe", "session", "show"]).expect("show parses");
    let command = super::classify(cli.command.as_ref()).expect("session show is a scoped read");
    let source = super::read_request_source(&cli);
    let outcome = super::execute(command, &source).await;
    assert_eq!(outcome.stdout, None);
    assert_eq!(
        outcome.stderr.as_deref(),
        Some("identifier required in daemon read mode\n")
    );
    assert_eq!(outcome.exit, 2);
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
            continue;
        };
        let canonical: &[&str] = match (case.command.as_str(), case.alias.as_deref()) {
            ("list", None) => &["aoe", "list"],
            ("list", Some("ls")) => &["aoe", "ls"],
            ("status", _) => &["aoe", "status"],
            ("session-show", _) => &["aoe", "session", "show", "s-1"],
            ("session-list-trash", _) => &["aoe", "session", "list-trash"],
            ("group-list", None) => &["aoe", "group", "list"],
            ("group-list", Some("group-ls")) => &["aoe", "group", "ls"],
            ("profile-bare", _) => &["aoe", "profile"],
            ("profile-list", None) => &["aoe", "profile", "list"],
            ("profile-list", Some("profile-ls")) => &["aoe", "profile", "ls"],
            ("project-list", None) => &["aoe", "project", "list"],
            ("project-list", Some("project-ls")) => &["aoe", "project", "ls"],
            (other, _) => panic!("unknown command/alias {other}"),
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

#[tokio::test]
#[serial_test::parallel]
async fn a_redirect_or_error_upgrade_is_unavailable() {
    for response in [REDIRECT, SERVER_ERROR] {
        let error = upgrade_against(response)
            .await
            .expect_err("a non-101 upgrade must not connect");
        assert_eq!(map_upgrade_error(error).code(), "unavailable");
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
