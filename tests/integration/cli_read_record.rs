//! Re-records the affected `tests/fixtures/cli-read` transcripts from the real
//! producer, then re-seals the pack through its own tooling.
//!
//! This is the recording half of the Contract Pack's wire gate. The three
//! refusal cases carry no application frames and are never touched; the three
//! that carry a Hello and a Snapshot are rewritten from
//! [`record_exchange`], which is the same assembly path and the same
//! serialisers the daemon uses. The HTTP upgrade records and the close
//! handshake around those frames are transport bytes rather than producer
//! frames, so they are carried over unchanged: the drift this repairs is in
//! the JSON documents, not in the handshake.
//!
//! Run it deliberately:
//!
//! ```text
//! cargo test --test integration cli_read_record -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use agent_of_empires::cli::runtime_read::pack;
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

/// The instant every recorded timestamp is stamped with. A whole second, so the
/// transcript is reproducible; the fractional spelling is what the schema now
/// admits, and the gate proves it against a deliberately malformed frame.
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
    previous: Option<std::ffi::OsString>,
}

impl TempAppDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp app dir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        Self {
            _dir: dir,
            previous,
        }
    }
}

impl Drop for TempAppDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
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

/// An unmasked server-to-client text frame, which is what the producer writes:
/// the mask direction is part of the contract and the pack verifies it.
fn text_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x81];
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(payload);
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

fn collect(root: &Path, dir: &Path, found: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).expect("read pack dir") {
        let entry = entry.expect("pack entry");
        let path = entry.path();
        if entry.file_type().expect("entry type").is_dir() {
            collect(root, &path, found);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("below root")
                .to_string_lossy()
                .into_owned();
            found.push((relative, path));
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn rewrite_manifest(root: &Path) {
    let mut lines: Vec<(String, PathBuf)> = Vec::new();
    collect(root, root, &mut lines);
    lines.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let text: String = lines
        .into_iter()
        .filter(|(path, _)| path != pack::MANIFEST_NAME)
        .map(|(path, full)| {
            format!(
                "{}  {path}\n",
                digest(&fs::read(&full).expect("read pack file"))
            )
        })
        .collect();
    fs::write(root.join(pack::MANIFEST_NAME), text).expect("rewrite manifest");
}

fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => serde_json::to_string(text).expect("string encodes"),
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", body.join(","))
        }
        serde_json::Value::Object(entries) => {
            let mut keys: Vec<&String> = entries.keys().collect();
            keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            let body: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key encodes"),
                        canonical(&entries[*key])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
    }
}

/// Bring `CASES.json` back into agreement with the bytes on disk, then re-hash
/// the manifest: the same two steps `pack::verify` checks, in the same order,
/// so a re-seal can never paper over a case that still disagrees.
fn restage(root: &Path) {
    let path = root.join(pack::CASES_NAME);
    let text = fs::read_to_string(&path).expect("read CASES.json");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("parse CASES.json");
    for case in value["cases"].as_array_mut().expect("cases array") {
        for key in ["all_files", "input_files", "output_files"] {
            let mut kept: Vec<serde_json::Value> = Vec::new();
            for reference in case[key].as_array().expect("file ref array") {
                let full = root.join(reference["path"].as_str().expect("ref path"));
                let Ok(bytes) = fs::read(&full) else {
                    continue;
                };
                let mut reference = reference.clone();
                reference["sha256"] = serde_json::json!(digest(&bytes));
                reference["bytes"] = serde_json::json!(bytes.len());
                kept.push(reference);
            }
            kept.sort_by(|left, right| {
                left["path"]
                    .as_str()
                    .expect("ref path")
                    .as_bytes()
                    .cmp(right["path"].as_str().expect("ref path").as_bytes())
            });
            case[key] = serde_json::Value::Array(kept);
        }
    }
    fs::write(&path, canonical(&value).as_bytes()).expect("rewrite CASES.json");
    rewrite_manifest(root);
}

/// The one thing a producer will not emit: a snapshot whose `default_profile`
/// names a profile the snapshot does not carry. The producer derives that
/// field from the same enumeration it publishes, so it cannot contradict
/// itself: which is exactly why the case has to plant the contradiction, and
/// why it has to plant it *after* the real frames are recorded rather than
/// typing the whole frame by hand.
const INVALID_DEFAULT_PROFILE: (&str, &str) = (
    "\"default_profile\":\"main\"",
    "\"default_profile\":\"absent\"",
);

/// A recorded `wire.raw`, split into records and re-emitted with the framing
/// headers recomputed, so one record's bytes can change without the record
/// length lying about them.
fn retranscribe(bytes: &[u8], mut edit: impl FnMut(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut records = 0usize;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(
            bytes[offset + 2..offset + 6]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        let payload = edit(&bytes[offset + 6..offset + 6 + length]);
        out.extend_from_slice(&bytes[offset..offset + 2]);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        offset += 6 + length;
        records += 1;
    }
    assert!(records > 0, "the transcript has records to edit");
    out
}

/// Replace a string inside the JSON body of a recorded frame. The edit is on
/// the body rather than on the record bytes because a WebSocket header is not
/// text, and the edited body is re-framed rather than spliced in place because
/// the header carries the body's length.
fn splice_text(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    let mut seen = false;
    let spliced = retranscribe(bytes, |payload| {
        if !payload.first().is_some_and(|first| first & 0x0F == 0x1) {
            return payload.to_vec();
        }
        let body = match payload[1] & 0x7F {
            126 => &payload[4..],
            127 => &payload[10..],
            _ => &payload[2..],
        };
        let text = std::str::from_utf8(body).expect("a frame body is JSON text");
        if !text.contains(from) {
            return payload.to_vec();
        }
        seen = true;
        text_frame(text.replace(from, to).as_bytes())
    });
    assert!(seen, "the transcript carries {from:?} to replace");
    spliced
}

#[test]
#[ignore = "rewrites the committed transcripts; run deliberately"]
fn re_record_the_affected_transcripts() {
    let root = pack::pack_root();
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
    restage(&root);
    pack::verify(&root).expect("the re-sealed pack verifies");
}

/// The stale-default case, recorded rather than typed: the store is seeded
/// with a resolved default it does not have and the frames come from the same
/// [`record_exchange`] the other cases use. The transport records are carried
/// over from the nominal `status` case, which is the same HTTP upgrade and the
/// same WebSocket handshake, so nothing about the handshake is invented here.
/// The refusal is the one the local resolver prints for the same state, and
/// that sentence is the whole claim.
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
    let stderr = format!(
        "Error: Profile '{MISSING_DEFAULT}' does not exist. Create it with: aoe profile create {MISSING_DEFAULT}\n"
    );
    fs::write(dir.join("expected.stdout"), b"").expect("a refusal prints no rows");
    fs::write(dir.join("expected.stderr"), &stderr).expect("write the refusal");
    fs::write(
        dir.join("expected.json"),
        canonical(&serde_json::json!({
            "artifact_revision": pack::ARTIFACT_REVISION,
            "case_id": DEFAULT_MISSING,
            "code": "default_missing",
            "exit": 1,
            "phase": "semantic",
            "stderr_sha256": digest(stderr.as_bytes()),
            "stdout_sha256": digest(b""),
            "wire_frames": 2,
        }))
        .as_bytes(),
    )
    .expect("write the expected result");
    fs::write(
        dir.join("error.json"),
        canonical(&serde_json::json!({
            "artifact_revision": pack::ARTIFACT_REVISION,
            "code": "default_missing",
            "message": stderr,
            "phase": "semantic",
        }))
        .as_bytes(),
    )
    .expect("write the refusal metadata");

    // The index row carries placeholder refs; `restage` reads every file on
    // disk and rewrites them, exactly as it does for the cases already listed.
    let ref_for = |name: &str| {
        serde_json::json!({
            "path": format!("cases/{DEFAULT_MISSING}/{name}"),
            "sha256": "0".repeat(64),
            "bytes": 0,
        })
    };
    let refs = |names: &[&str]| names.iter().map(|name| ref_for(name)).collect::<Vec<_>>();
    let cases_path = root.join(pack::CASES_NAME);
    let mut index: serde_json::Value =
        serde_json::from_slice(&fs::read(&cases_path).expect("read CASES.json"))
            .expect("parse CASES.json");
    let cases = index["cases"].as_array_mut().expect("cases array");
    cases.retain(|case| case["case_id"] != serde_json::json!(DEFAULT_MISSING));
    cases.push(serde_json::json!({
        "alias": serde_json::Value::Null,
        "all_files": refs(&[
            "error.json", "expected.json", "expected.stderr",
            "expected.stdout", "wire.raw",
        ]),
        "argv": ["aoe", "status"],
        "auth": "bearer_valid",
        "case_id": DEFAULT_MISSING,
        "command": "status",
        "input_files": refs(&["wire.raw"]),
        "output_files": refs(&[
            "error.json", "expected.json", "expected.stderr", "expected.stdout",
        ]),
        "parse": "ok",
        "replay_role": "server_mode",
        "transport": "http_loopback",
    }));
    fs::write(&cases_path, canonical(&index).as_bytes()).expect("rewrite CASES.json");
    println!("{DEFAULT_MISSING}: {} bytes", recorded.len());
}
