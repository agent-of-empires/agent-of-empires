//! Re-record producer frames without changing frozen stdout/stderr expectations.
//! Run: cargo test --test integration cli_read_record -- --ignored --nocapture

use std::fs;
use std::path::{Path, PathBuf};

use agent_of_empires::server::test_support::{
    record_exchange, seed_instances_on_disk_for_test, RecordedExchange, RecordedOwner,
    RecordingPins,
};
use agent_of_empires::session::{self, Instance, Status};

/// The cases that carry application frames, and the owner each exchange
/// declares. The three refusal cases are absent by construction: they hold only
/// HTTP request and response text, so no producer frame, no project row and no
/// timestamp ever appears in them.
const AFFECTED: [(&str, RecordedOwner); 3] = [
    ("uds-list-nominal", RecordedOwner::Local { uid: 501 }),
    ("http-loopback-status-nominal", RecordedOwner::Remote),
    (
        "http-loopback-snapshot-schema-invalid",
        RecordedOwner::Remote,
    ),
];

/// Fixed UTC instant keeps recorded fixtures reproducible.
const RECORDED_AT: &str = "2026-01-01T00:00:00Z";

/// A case the producer does not emit on its own either: a snapshot whose
/// resolved default names a profile the store does not have. It is recorded
/// from the live producer with a store seeded that way, and it carries the
/// refusal that state draws, which is the local resolver's own sentence at
/// exit 1 rather than a wire code naming nothing.
const DEFAULT_MISSING: &str = "http-loopback-default-missing";
/// The default the recorded store resolves to and does not have.
const MISSING_DEFAULT: &str = "retired";

struct TempAppDir {
    _dir: tempfile::TempDir,
    _env: agent_of_empires::server::test_support::RuntimeEnvGuard,
}

impl TempAppDir {
    fn new() -> Self {
        let mut env = agent_of_empires::server::test_support::RuntimeEnvGuard::read_lock();
        let dir = tempfile::tempdir().expect("temp app dir");
        env.bind(dir.path());
        Self {
            _dir: dir,
            _env: env,
        }
    }
}

/// The store the recorded frames describe: one profile, a parent and a child
/// group, a registered project, and a single live session that resolves through
/// both. The registry project is the point: it is the only source of
/// `registered: true`, and a row synthesized for a session's project path is
/// the only source of `false`.
fn seed() -> Vec<Instance> {
    seed_with_default("main")
}

/// The same store with a resolved default the store does not have, which is
/// the one state the producer publishes and the client has to refuse by name.
fn seed_with_default(default: &str) -> Vec<Instance> {
    session::create_profile("main").expect("create the recorded profile");
    fs::write(
        session::get_app_dir().expect("app dir").join("config.toml"),
        format!("default_profile = \"{default}\"\n"),
    )
    .expect("seed the resolved default");
    let profile_dir = session::get_app_dir()
        .expect("app dir")
        .join("profiles")
        .join("main");
    fs::write(
        profile_dir.join("groups.json"),
        serde_json::to_vec_pretty(&serde_json::json!([
            {"name": "work", "path": "work", "collapsed": false},
            {"name": "deep", "path": "work/deep", "collapsed": false},
        ]))
        .expect("groups"),
    )
    .expect("seed groups");
    fs::write(
        profile_dir.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!([
            {"name": "alpha", "path": "/srv/alpha", "scope": "profile"},
        ]))
        .expect("projects"),
    )
    .expect("seed projects");

    let mut instance = Instance::new("Alpha session", "/srv/alpha");
    instance.id = "s-1".into();
    instance.source_profile = "main".into();
    instance.group_path = "work/deep".into();
    instance.command = "claude".into();
    instance.tool = "claude".into();
    instance.status = Status::Waiting;
    instance.created_at = RECORDED_AT.parse().expect("recorded instant");
    seed_instances_on_disk_for_test("main", vec![instance.clone()]);
    vec![instance]
}

/// One `wire.raw` record: a six-byte header plus its payload.
struct Record {
    direction: u8,
    role: u8,
    bytes: Vec<u8>,
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.direction, self.role];
        out.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }
}

fn read_records(bytes: &[u8]) -> Vec<Record> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let direction = bytes[offset];
        let role = bytes[offset + 1];
        let length = u32::from_be_bytes(
            bytes[offset + 2..offset + 6]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        records.push(Record {
            direction,
            role,
            bytes: bytes[offset + 6..offset + 6 + length].to_vec(),
        });
        offset += 6 + length;
    }
    records
}

fn text_frame(payload: &[u8]) -> Vec<u8> {
    use tokio_tungstenite::tungstenite::protocol::frame::{
        coding::{Data, OpCode},
        Frame,
    };
    let mut out = Vec::new();
    Frame::message(payload.to_vec(), OpCode::Data(Data::Text), true)
        .format(&mut out)
        .unwrap();
    out
}

/// Replace the producer's application frames in a recorded transcript, keeping
/// every transport record around them.
fn splice(case: &str, previous: &[u8], exchange: &RecordedExchange) -> Vec<u8> {
    let frames = [exchange.hello.clone(), exchange.snapshot.clone()];
    let mut seen = 0usize;
    let mut out = Vec::new();
    for record in read_records(previous) {
        let is_text =
            record.direction == 2 && record.bytes.first().is_some_and(|b| b & 0x0F == 0x1);
        if is_text {
            out.extend(
                Record {
                    direction: record.direction,
                    role: record.role,
                    bytes: text_frame(&frames[seen]),
                }
                .encode(),
            );
            seen += 1;
            continue;
        }
        out.extend(record.encode());
    }
    assert_eq!(seen, frames.len(), "case {case} had no application frames");
    out
}

// Plant a dangling profile only after recording the real producer output.
const INVALID_DEFAULT_PROFILE: (&str, &str) = (
    "\"default_profile\":\"main\"",
    "\"default_profile\":\"absent\"",
);

fn splice_text(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    use tokio_tungstenite::tungstenite::{protocol::Role, Message, WebSocket};
    let mut seen = false;
    let mut out = Vec::new();
    for mut record in read_records(bytes) {
        if record.direction == 2 && !record.bytes.starts_with(b"HTTP/") {
            let mut socket = WebSocket::from_raw_socket(
                std::io::Cursor::new(record.bytes.clone()),
                Role::Client,
                None,
            );
            if let Message::Text(text) = socket.read().unwrap() {
                if text.contains(from) {
                    seen = true;
                    record.bytes = text_frame(text.replace(from, to).as_bytes());
                }
            }
        }
        out.extend(record.encode());
    }
    assert!(seen, "the transcript carries the semantic field to edit");
    out
}

#[test]
#[serial_test::serial]
#[ignore = "rewrites the committed transcripts; run deliberately"]
fn re_record_the_affected_transcripts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli-read");
    let pins = || RecordingPins {
        runtime_epoch: "3f2b1c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d".into(),
        prebind_instance_id: "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d".into(),
        runtime_instance_id: "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5e".into(),
        observed_at: RECORDED_AT.parse().expect("recorded instant"),
    };
    for (case, owner) in AFFECTED {
        let _app_dir = TempAppDir::new();
        let instances = seed();
        let exchange = record_exchange(&instances, owner, &pins());

        let path = root.join("cases").join(case).join("wire.raw");
        let previous = fs::read(&path).expect("read the recorded transcript");
        let mut recorded = splice(case, &previous, &exchange);
        if case == "http-loopback-snapshot-schema-invalid" {
            let (from, to) = INVALID_DEFAULT_PROFILE;
            recorded = splice_text(&recorded, from, to);
        }
        fs::write(&path, &recorded).expect("write the re-recorded transcript");
        println!("{case}: {} bytes", recorded.len());
    }
    record_default_missing(&root, pins());
}

// Preserve the existing refusal goldens while updating the producer frames.
fn record_default_missing(root: &Path, pins: RecordingPins) {
    let _app_dir = TempAppDir::new();
    let instances = seed_with_default(MISSING_DEFAULT);
    let exchange = record_exchange(&instances, RecordedOwner::Remote, &pins);
    let template = fs::read(root.join("cases/http-loopback-status-nominal/wire.raw"))
        .expect("read the nominal transcript");
    let recorded = splice(DEFAULT_MISSING, &template, &exchange);

    let dir = root.join("cases").join(DEFAULT_MISSING);
    fs::create_dir_all(&dir).expect("the new case directory");
    fs::write(dir.join("wire.raw"), &recorded).expect("write the transcript");
}
