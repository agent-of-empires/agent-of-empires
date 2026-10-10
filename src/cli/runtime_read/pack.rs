//! Recorded wire fixtures and the published v4 data schemas, for tests only.

use std::fs;
use std::io::{self, Cursor};
use std::path::PathBuf;
use std::sync::LazyLock;
use tokio_tungstenite::tungstenite::{protocol::Role, Message, Utf8Bytes, WebSocket};

pub(crate) const HELLO_SCHEMA: &str = "hello.schema.json";
pub(crate) const SNAPSHOT_SCHEMA: &str = "snapshot.schema.json";

pub(super) struct Fixture {
    pub wire: Vec<WireRecord>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(super) struct WireRecord {
    pub direction: u8,
    pub bytes: Vec<u8>,
}

pub(super) fn fixture(name: &str) -> Fixture {
    let dir = pack_root().join("cases").join(name);
    Fixture {
        wire: parse_wire(&fs::read(dir.join("wire.raw")).expect("recorded wire"))
            .expect("complete wire records"),
        stdout: fs::read(dir.join("expected.stdout")).expect("stdout golden"),
        stderr: fs::read(dir.join("expected.stderr")).expect("stderr golden"),
    }
}

fn parse_wire(mut bytes: &[u8]) -> io::Result<Vec<WireRecord>> {
    let mut records = Vec::new();
    while !bytes.is_empty() {
        let header = bytes
            .get(..6)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated record header"))?;
        if !matches!((header[0], header[1]), (1, 1) | (2, 2)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid record direction or role",
            ));
        }
        let length = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
        let payload = bytes[6..].get(..length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "truncated record payload")
        })?;
        records.push(WireRecord {
            direction: header[0],
            bytes: payload.to_vec(),
        });
        bytes = &bytes[6 + length..];
    }
    Ok(records)
}

pub(super) fn application_frames(records: Vec<WireRecord>) -> Vec<Utf8Bytes> {
    records
        .into_iter()
        .filter_map(|record| {
            if record.direction != 2 || record.bytes.starts_with(b"HTTP/") {
                return None;
            }
            let mut socket =
                WebSocket::from_raw_socket(Cursor::new(record.bytes), Role::Client, None);
            match socket.read().expect("recorded WebSocket frame") {
                Message::Text(text) => {
                    let frame: serde_json::Value =
                        serde_json::from_slice(text.as_bytes()).expect("JSON frame");
                    let schema = match frame["kind"].as_str() {
                        Some("hello") => HELLO_SCHEMA,
                        Some("snapshot") => SNAPSHOT_SCHEMA,
                        _ => panic!("unexpected application frame"),
                    };
                    validate_data(schema, &frame["data"])
                        .expect("recorded data satisfies its published schema");
                    Some(text)
                }
                Message::Close(_) | Message::Ping(_) | Message::Pong(_) => None,
                other => panic!("unexpected recorded message: {other:?}"),
            }
        })
        .collect()
}

pub(crate) fn pack_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli-read")
}

fn validator(name: &str) -> jsonschema::Validator {
    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(pack_root().join(name)).expect("published schema"))
            .expect("JSON schema");
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .expect("valid published schema")
}

static HELLO: LazyLock<jsonschema::Validator> = LazyLock::new(|| validator(HELLO_SCHEMA));
static SNAPSHOT: LazyLock<jsonschema::Validator> = LazyLock::new(|| validator(SNAPSHOT_SCHEMA));

pub(crate) fn validate_data(name: &str, data: &serde_json::Value) -> Result<(), String> {
    let validator = match name {
        HELLO_SCHEMA => &*HELLO,
        SNAPSHOT_SCHEMA => &*SNAPSHOT,
        _ => panic!("unknown wire schema"),
    };
    validator.validate(data).map_err(|error| error.to_string())
}
