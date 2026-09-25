//! The Contract Pack verifier: the sole gate that must pass before a
//! `tests/fixtures/cli-read` fixture is replayed.
//!
//! The pack is unsigned integrity governance (P1-06 is an accepted residual
//! risk), so everything here is about drift detection, not authenticity: the
//! manifest universe must match the physical tree, every digest must match,
//! `CASES.json` must be RFC 8785 canonical, each case directory must contain
//! exactly the files its row declares, and the expected result must agree with
//! the bytes on disk.
//!
//! The JSON Schema documents shipped beside the fixtures are the published
//! contract; the closed structs below enforce the same closure at run time, so
//! no JSON Schema engine is needed to gate a run.
//!
//! `root-home` is a replay scratch path, never manifest-listed: a harness that
//! materializes it must remove the subtree before calling [`verify`].

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Manifest name. Covers every other physical file in the pack.
pub const MANIFEST_NAME: &str = "MANIFEST.sha256";
/// Canonical case index.
pub const CASES_NAME: &str = "CASES.json";
/// The artifact revision every metadata document is pinned to.
pub const ARTIFACT_REVISION: &str = "v151";

/// Per-file `wire.raw` record header: direction, role, big-endian length.
const RECORD_HEADER: usize = 6;
const DIRECTION_CLIENT_TO_SERVER: u8 = 1;
const DIRECTION_SERVER_TO_CLIENT: u8 = 2;
const ROLE_CLIENT: u8 = 1;
const ROLE_SERVER: u8 = 2;

#[derive(Debug)]
pub struct PackError(String);

impl fmt::Display for PackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PackError {}

type Result<T, E = PackError> = std::result::Result<T, E>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(PackError(message.into()))
}

/// One `wire.raw` record: a complete six-byte header plus its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRecord {
    pub direction: u8,
    pub role: u8,
    pub bytes: Vec<u8>,
}

impl WireRecord {
    pub fn is_client_to_server(&self) -> bool {
        self.direction == DIRECTION_CLIENT_TO_SERVER
    }

    pub fn is_server_to_client(&self) -> bool {
        self.direction == DIRECTION_SERVER_TO_CLIENT
    }

    /// The complete record bytes: its six-byte header plus its payload.
    pub fn raw(&self) -> Vec<u8> {
        let mut out = vec![self.direction, self.role];
        out.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    /// The endpoint whose records this case compares (`replay_role`).
    pub fn is_compared_role(&self, replay_role: &str) -> bool {
        self.role == role_for(replay_role)
    }
}

/// A case that passed every gate, ready to replay.
#[derive(Debug)]
pub struct VerifiedCase {
    pub case_id: String,
    pub command: String,
    pub alias: Option<String>,
    pub parse: String,
    pub transport: String,
    pub auth: String,
    pub argv: Vec<String>,
    pub replay_role: String,
    pub phase: String,
    pub code: Option<String>,
    pub exit: u8,
    pub wire_frames: u64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wire: Vec<WireRecord>,
}

#[derive(Debug)]
pub struct VerifiedPack {
    pub cases: Vec<VerifiedCase>,
}

impl VerifiedPack {
    pub fn case(&self, case_id: &str) -> Option<&VerifiedCase> {
        self.cases.iter().find(|case| case.case_id == case_id)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CasesFile {
    schema: u8,
    artifact_revision: String,
    cases: Vec<CaseRow>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct CaseRow {
    case_id: String,
    command: String,
    alias: Option<String>,
    parse: String,
    transport: String,
    auth: String,
    argv: Vec<String>,
    replay_role: String,
    input_files: Vec<FileRef>,
    output_files: Vec<FileRef>,
    all_files: Vec<FileRef>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FileRef {
    path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedFile {
    artifact_revision: String,
    case_id: String,
    phase: String,
    code: Option<String>,
    exit: u8,
    stdout_sha256: String,
    stderr_sha256: String,
    wire_frames: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorFile {
    artifact_revision: String,
    phase: String,
    code: String,
    message: String,
}

fn role_for(replay_role: &str) -> u8 {
    match replay_role {
        "server_mode" => ROLE_SERVER,
        _ => ROLE_CLIENT,
    }
}

/// The closed command/alias matrix: a command admits only its own aliases.
fn alias_allowed(command: &str, alias: Option<&str>) -> bool {
    let allowed: &[&str] = match command {
        "list" => &["ls"],
        "status" | "session-show" | "session-list-trash" | "profile-bare" => &[],
        "group-list" => &["group-ls"],
        "profile-list" => &["profile-ls"],
        "project-list" => &["project-ls"],
        "unparsable" => &[],
        _ => return false,
    };
    match alias {
        None => true,
        Some(value) => allowed.contains(&value),
    }
}

/// The exhaustive phase/code/exit table. `code` is `None` only for a
/// successful render, which exits 0.
fn expected_exit(phase: &str, code: Option<&str>) -> Option<u8> {
    let table: &[(&str, &[&str], u8)] = &[
        ("parser", &["parser_error", "identifier_required"], 2),
        ("handshake", &["schema_invalid"], 4),
        (
            "pre_transport",
            &[
                "invalid_endpoint",
                "invalid_token",
                "establishment_timeout",
                "marker_missing",
                "marker_invalid",
                "marker_identity",
                "anchored_alias_unavailable",
            ],
            2,
        ),
        (
            "transport",
            &[
                "unauthorized",
                "unavailable",
                "socket_identity",
                "peer_identity",
                "protocol_mismatch",
                "frame_limit",
                "connection_closed",
            ],
            4,
        ),
        ("snapshot", &["schema_invalid"], 4),
        (
            "semantic",
            &[
                "health_degraded",
                "freshness_unavailable",
                "default_missing",
                "profile_missing",
                "session_missing",
                "session_ambiguous",
            ],
            4,
        ),
        ("close", &["close_timeout"], 4),
        ("renderer", &["renderer_internal"], 1),
    ];
    match (phase, code) {
        ("renderer", None) => Some(0),
        (_, Some(code)) => table
            .iter()
            .find(|(row_phase, codes, _)| *row_phase == phase && codes.contains(&code))
            .map(|(_, _, exit)| *exit),
        _ => None,
    }
}

const COMMANDS: &[&str] = &[
    "list",
    "status",
    "session-show",
    "session-list-trash",
    "group-list",
    "profile-bare",
    "profile-list",
    "project-list",
    "unparsable",
];
const ALIASES: &[&str] = &["ls", "group-ls", "profile-ls", "project-ls"];
const TRANSPORTS: &[&str] = &["uds", "http_loopback", "https"];
const AUTHS: &[&str] = &["none", "bearer_valid", "bearer_invalid", "bearer_missing"];
const REPLAY_ROLES: &[&str] = &["client_mode", "server_mode"];

fn valid_case_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    value.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A pack-relative path: ASCII, no leading/trailing/repeated separator, and no
/// component that is empty, a dot, or a backslash.
fn valid_relative_path(value: &str) -> bool {
    if value.is_empty() || value.starts_with('/') || value.ends_with('/') || value.contains("//") {
        return false;
    }
    value.split('/').all(|component| {
        !component.is_empty()
            && component != "."
            && component != ".."
            && component.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
    })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn digest(bytes: &[u8]) -> String {
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

/// Every physical file below `root`, as pack-relative paths. A symlink or any
/// other non-regular entry is refused rather than followed.
fn enumerate(root: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) -> Result<()> {
        let entries = fs::read_dir(dir)
            .map_err(|error| PackError(format!("cannot read {}: {error}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                PackError(format!(
                    "cannot read an entry in {}: {error}",
                    dir.display()
                ))
            })?;
            let kind = entry.file_type().map_err(|error| {
                PackError(format!("cannot stat {}: {error}", entry.path().display()))
            })?;
            if !kind.is_dir() && !kind.is_file() {
                return fail(format!(
                    "pack contains a non-regular entry: {}",
                    entry.path().display()
                ));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = dir.join(&name);
            if kind.is_dir() {
                walk(root, &child, found)?;
            } else {
                found.push(
                    child
                        .strip_prefix(root)
                        .expect("child is below root")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(root, root, &mut found)?;
    found.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(found)
}

fn read(root: &Path, relative: &str) -> Result<Vec<u8>> {
    fs::read(root.join(relative))
        .map_err(|error| PackError(format!("cannot read {relative}: {error}")))
}

/// Parse `MANIFEST.sha256` and return its path/digest pairs, requiring the
/// bytewise-sorted, duplicate-free, self-excluding form.
fn parse_manifest(bytes: &[u8], physical: &[String]) -> Result<BTreeMap<String, String>> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| PackError("MANIFEST.sha256 is not UTF-8".into()))?;
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    let mut previous: Option<&str> = None;
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            return fail(format!("MANIFEST.sha256 line {} is empty", index + 1));
        }
        let (hash, path) = line
            .split_once("  ")
            .ok_or_else(|| PackError(format!("MANIFEST.sha256 line {} is malformed", index + 1)))?;
        if !valid_digest(hash) {
            return fail(format!(
                "MANIFEST.sha256 line {} has a bad digest",
                index + 1
            ));
        }
        if !valid_relative_path(path) {
            return fail(format!("MANIFEST.sha256 line {} has a bad path", index + 1));
        }
        if path == MANIFEST_NAME {
            return fail("MANIFEST.sha256 must not list itself");
        }
        if previous.is_some_and(|previous| previous.as_bytes() >= path.as_bytes()) {
            return fail(format!(
                "MANIFEST.sha256 line {} is not bytewise sorted",
                index + 1
            ));
        }
        previous = Some(path);
        entries.insert(path.to_string(), hash.to_string());
    }
    // The physical universe, minus the manifest, must be exactly what is listed.
    let listed: Vec<&String> = entries.keys().collect();
    let actual: Vec<&String> = physical
        .iter()
        .filter(|path| path.as_str() != MANIFEST_NAME)
        .collect();
    if listed != actual.as_slice() {
        let missing: Vec<&str> = actual
            .iter()
            .filter(|path| !entries.contains_key(path.as_str()))
            .map(|path| path.as_str())
            .collect();
        let extra: Vec<&str> = listed
            .iter()
            .filter(|path| {
                !physical
                    .iter()
                    .any(|actual| actual.as_str() == path.as_str())
            })
            .map(|path| path.as_str())
            .collect();
        return fail(format!(
            "manifest universe mismatch: unlisted {missing:?}, absent-from-disk {extra:?}"
        ));
    }
    Ok(entries)
}

fn check_canonical(bytes: &[u8], what: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| PackError(format!("{what} is not JSON: {error}")))?;
    if canonical_json(&value) != String::from_utf8_lossy(bytes) {
        return fail(format!("{what} is not RFC 8785 canonical"));
    }
    Ok(value)
}

/// RFC 8785 canonical form: object keys sorted by UTF-16 code unit, no
/// insignificant whitespace, minimal string escapes.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(true) => "true".into(),
        serde_json::Value::Bool(false) => "false".into(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => serde_json::to_string(text).expect("string encodes"),
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
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
                        canonical_json(&entries[*key])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
    }
}

fn check_file_refs(case_id: &str, label: &str, refs: &[FileRef]) -> Result<()> {
    for window in refs.windows(2) {
        if window[0].path.as_bytes() >= window[1].path.as_bytes() {
            return fail(format!(
                "case {case_id} {label} is not bytewise sorted by path"
            ));
        }
    }
    let prefix = format!("cases/{case_id}/");
    for reference in refs {
        if !valid_relative_path(&reference.path) || !reference.path.starts_with(&prefix) {
            return fail(format!(
                "case {case_id} {label} has an out-of-case path: {}",
                reference.path
            ));
        }
        if !valid_digest(&reference.sha256) {
            return fail(format!(
                "case {case_id} {label} has a bad digest for {}",
                reference.path
            ));
        }
    }
    Ok(())
}

fn parse_wire(bytes: &[u8], case_id: &str) -> Result<Vec<WireRecord>> {
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < RECORD_HEADER {
            return fail(format!("case {case_id} has a truncated wire.raw header"));
        }
        let direction = bytes[offset];
        let role = bytes[offset + 1];
        if direction != DIRECTION_CLIENT_TO_SERVER && direction != DIRECTION_SERVER_TO_CLIENT {
            return fail(format!(
                "case {case_id} has an unknown wire direction {direction}"
            ));
        }
        if role != ROLE_CLIENT && role != ROLE_SERVER {
            return fail(format!("case {case_id} has an unknown wire role {role}"));
        }
        let length = u32::from_be_bytes(
            bytes[offset + 2..offset + RECORD_HEADER]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        let start = offset + RECORD_HEADER;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| PackError(format!("case {case_id} has a truncated wire.raw record")))?;
        records.push(WireRecord {
            direction,
            role,
            bytes: bytes[start..end].to_vec(),
        });
        offset = end;
    }
    Ok(records)
}

/// Whether a record is an HTTP upgrade request/response rather than a
/// WebSocket frame.
fn is_http_record(payload: &[u8]) -> bool {
    payload.starts_with(b"HTTP/1.")
        || payload
            .iter()
            .position(|byte| *byte == b' ')
            .is_some_and(|space| space > 0 && payload[..space].iter().all(u8::is_ascii_alphabetic))
}

/// Data/continuation units in the compared role, counted statically from
/// `wire.raw` before any replay.
fn count_wire_frames(case_id: &str, records: &[WireRecord], replay_role: &str) -> Result<u64> {
    let role = role_for(replay_role);
    let mut units = 0u64;
    for record in records.iter().filter(|record| record.role == role) {
        if is_http_record(&record.bytes) {
            continue;
        }
        let opcode = websocket_opcode(case_id, record.direction, &record.bytes)?;
        if matches!(opcode, 0x0..=0x2) {
            units += 1;
        }
    }
    Ok(units)
}

/// Validate one WebSocket record's framing and return its opcode. The mask
/// direction is part of the contract: client-to-server records are masked,
/// server-to-client records never are.
fn websocket_opcode(case_id: &str, direction: u8, payload: &[u8]) -> Result<u8> {
    if payload.len() < 2 {
        return fail(format!("case {case_id} has a one-byte WebSocket record"));
    }
    let first = payload[0];
    if first & 0x70 != 0 {
        return fail(format!(
            "case {case_id} has a WebSocket record with RSV bits set"
        ));
    }
    let opcode = first & 0x0F;
    if opcode > 0x0A {
        return fail(format!(
            "case {case_id} has an unknown WebSocket opcode {opcode}"
        ));
    }
    if opcode >= 0x8 && first & 0x80 == 0 {
        return fail(format!("case {case_id} has a fragmented control frame"));
    }
    let masked = payload[1] & 0x80 != 0;
    let masked_expected = direction == DIRECTION_CLIENT_TO_SERVER;
    if masked != masked_expected {
        return fail(format!(
            "case {case_id} has a WebSocket record masked in the wrong direction"
        ));
    }
    let mut cursor = 2usize;
    let short = payload[1] & 0x7F;
    let length = match short {
        126 => {
            let raw = payload
                .get(cursor..cursor + 2)
                .ok_or_else(|| PackError(format!("case {case_id} has a short frame length")))?;
            cursor += 2;
            u16::from_be_bytes(raw.try_into().expect("two bytes")) as usize
        }
        127 => {
            let raw = payload
                .get(cursor..cursor + 8)
                .ok_or_else(|| PackError(format!("case {case_id} has a short frame length")))?;
            cursor += 8;
            u64::from_be_bytes(raw.try_into().expect("eight bytes")) as usize
        }
        other => other as usize,
    };
    if masked {
        cursor += 4;
    }
    if cursor + length != payload.len() {
        return fail(format!(
            "case {case_id} has a WebSocket record whose length disagrees with its bytes"
        ));
    }
    Ok(opcode)
}

/// Verify a whole pack: safe root, manifest universe and digests, canonical
/// `CASES.json`, per-case layout, and the expected/error closure.
pub fn verify(root: &Path) -> Result<VerifiedPack> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| PackError(format!("cannot stat the pack root: {error}")))?;
    if !metadata.is_dir() {
        return fail("pack root is not a directory");
    }
    let physical = enumerate(root)?;
    if !physical.iter().any(|path| path == MANIFEST_NAME) {
        return fail(format!("pack has no {MANIFEST_NAME}"));
    }
    let manifest = parse_manifest(&read(root, MANIFEST_NAME)?, &physical)?;
    for (path, expected) in &manifest {
        let actual = digest(&read(root, path)?);
        if &actual != expected {
            return fail(format!("{path} does not match its manifest digest"));
        }
    }

    let cases_bytes = read(root, CASES_NAME)?;
    let value = check_canonical(&cases_bytes, CASES_NAME)?;
    let cases: CasesFile = serde_json::from_value(value).map_err(|error| {
        PackError(format!(
            "{CASES_NAME} does not close over its contract: {error}"
        ))
    })?;
    if cases.schema != 1 || cases.artifact_revision != ARTIFACT_REVISION {
        return fail(format!(
            "{CASES_NAME} is not schema 1 revision {ARTIFACT_REVISION}"
        ));
    }

    let mut seen: Vec<&str> = Vec::new();
    for row in &cases.cases {
        if !valid_case_id(&row.case_id) {
            return fail(format!(
                "case_id {:?} is not a single valid component",
                row.case_id
            ));
        }
        if seen.contains(&row.case_id.as_str()) {
            return fail(format!("duplicate case_id {}", row.case_id));
        }
        seen.push(&row.case_id);
    }

    let mut verified = Vec::with_capacity(cases.cases.len());
    for row in &cases.cases {
        verified.push(verify_case(root, &physical, row)?);
    }
    Ok(VerifiedPack { cases: verified })
}

fn verify_case(root: &Path, physical: &[String], row: &CaseRow) -> Result<VerifiedCase> {
    let case_id = &row.case_id;
    if !COMMANDS.contains(&row.command.as_str())
        || row
            .alias
            .as_deref()
            .is_some_and(|alias| !ALIASES.contains(&alias))
        || !alias_allowed(&row.command, row.alias.as_deref())
    {
        return fail(format!(
            "case {case_id} has a command/alias outside the matrix"
        ));
    }
    if !["ok", "parser_error"].contains(&row.parse.as_str())
        || !TRANSPORTS.contains(&row.transport.as_str())
        || !AUTHS.contains(&row.auth.as_str())
        || !REPLAY_ROLES.contains(&row.replay_role.as_str())
    {
        return fail(format!(
            "case {case_id} has an unknown parse/transport/auth/replay_role"
        ));
    }
    if row.argv.first().map(String::as_str) != Some("aoe") {
        return fail(format!(
            "case {case_id} argv must start with the binary name"
        ));
    }

    let prefix = format!("cases/{case_id}/");
    // The per-case layout is read from the filesystem, not from the row, so a
    // file that is on disk but undeclared (or declared but absent) is drift.
    let mut on_disk: Vec<String> = physical
        .iter()
        .filter_map(|path| path.strip_prefix(&prefix).map(str::to_string))
        .collect();
    on_disk.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut expected_names: Vec<String> = vec![
        "expected.json".into(),
        "expected.stderr".into(),
        "expected.stdout".into(),
        "wire.raw".into(),
    ];
    // `wire.jsonl` is the optional decoded view; `error.json` rides on the exit.
    if on_disk.iter().any(|name| name == "wire.jsonl") {
        expected_names.push("wire.jsonl".into());
    }
    if on_disk.iter().any(|name| name == "error.json") {
        expected_names.push("error.json".into());
    }
    expected_names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if on_disk != expected_names {
        return fail(format!(
            "case {case_id} layout is {on_disk:?}, expected {expected_names:?}"
        ));
    }
    let declared: Vec<String> = row
        .all_files
        .iter()
        .map(|reference| reference.path.clone())
        .collect();
    let physical_refs: Vec<String> = on_disk
        .iter()
        .map(|name| format!("{prefix}{name}"))
        .collect();
    if declared != physical_refs {
        return fail(format!(
            "case {case_id} all_files is {declared:?}, which is not its directory"
        ));
    }

    for (label, refs) in [
        ("all_files", &row.all_files),
        ("input_files", &row.input_files),
        ("output_files", &row.output_files),
    ] {
        check_file_refs(case_id, label, refs)?;
    }
    let known: Vec<&FileRef> = row.all_files.iter().collect();
    for (label, refs) in [
        ("input_files", &row.input_files),
        ("output_files", &row.output_files),
    ] {
        for reference in refs {
            if !known.contains(&reference) {
                return fail(format!(
                    "case {case_id} {label} references {:?}, which all_files does not list",
                    reference.path
                ));
            }
        }
    }
    for reference in &row.all_files {
        let bytes = read(root, &reference.path)?;
        if bytes.len() as u64 != reference.bytes || digest(&bytes) != reference.sha256 {
            return fail(format!(
                "case {case_id} FileRef for {} does not match the file",
                reference.path
            ));
        }
    }

    let stdout = read(root, &format!("{prefix}expected.stdout"))?;
    let stderr = read(root, &format!("{prefix}expected.stderr"))?;
    let expected_bytes = read(root, &format!("{prefix}expected.json"))?;
    let expected_value =
        check_canonical(&expected_bytes, &format!("case {case_id} expected.json"))?;
    let expected: ExpectedFile = serde_json::from_value(expected_value).map_err(|error| {
        PackError(format!(
            "case {case_id} expected.json does not close over its contract: {error}"
        ))
    })?;
    if expected.artifact_revision != ARTIFACT_REVISION || expected.case_id != *case_id {
        return fail(format!(
            "case {case_id} expected.json identifies another case or revision"
        ));
    }
    if !valid_digest(&expected.stdout_sha256) || !valid_digest(&expected.stderr_sha256) {
        return fail(format!("case {case_id} expected.json has a malformed hash"));
    }
    let Some(exit) = expected_exit(&expected.phase, expected.code.as_deref()) else {
        return fail(format!(
            "case {case_id} pairs phase {} with code {:?}, which the matrix forbids",
            expected.phase, expected.code
        ));
    };
    if expected.exit != exit {
        return fail(format!(
            "case {case_id} exits {} but its phase/code pair requires {exit}",
            expected.exit
        ));
    }
    if digest(&stdout) != expected.stdout_sha256 || digest(&stderr) != expected.stderr_sha256 {
        return fail(format!(
            "case {case_id} expected.json hashes do not match expected.stdout/expected.stderr"
        ));
    }
    if expected.exit != 0 && !stdout.is_empty() {
        return fail(format!("case {case_id} is a failure but carries stdout"));
    }

    let error_path = root.join(format!("{prefix}error.json"));
    match (expected.exit, error_path.exists()) {
        (0, true) => return fail(format!("case {case_id} succeeds but ships error.json")),
        (0, false) => {}
        (_, false) => {
            return fail(format!(
                "case {case_id} exits {} but has no error.json",
                expected.exit
            ))
        }
        (_, true) => {
            let bytes = fs::read(&error_path).expect("error.json was just stat'ed");
            let value = check_canonical(&bytes, &format!("case {case_id} error.json"))?;
            let error: ErrorFile = serde_json::from_value(value).map_err(|parse| {
                PackError(format!(
                    "case {case_id} error.json does not close over its contract: {parse}"
                ))
            })?;
            if error.artifact_revision != ARTIFACT_REVISION
                || error.phase != expected.phase
                || Some(error.code.as_str()) != expected.code.as_deref()
            {
                return fail(format!(
                    "case {case_id} error.json disagrees with expected.json"
                ));
            }
            if error.message.as_bytes() != stderr.as_slice() {
                return fail(format!(
                    "case {case_id} error.json message is not byte-identical to expected.stderr"
                ));
            }
        }
    }

    let wire = parse_wire(&read(root, &format!("{prefix}wire.raw"))?, case_id)?;
    let wire_frames = count_wire_frames(case_id, &wire, &row.replay_role)?;
    if wire_frames != expected.wire_frames {
        return fail(format!(
            "case {case_id} counts {wire_frames} wire frames, expected.json says {}",
            expected.wire_frames
        ));
    }
    if row.transport == "uds"
        && wire
            .iter()
            .any(|record| record.is_client_to_server() && is_http(record))
    {
        return fail(format!(
            "case {case_id} is UDS but records an HTTP exchange"
        ));
    }

    Ok(VerifiedCase {
        case_id: case_id.clone(),
        command: row.command.clone(),
        alias: row.alias.clone(),
        parse: row.parse.clone(),
        transport: row.transport.clone(),
        auth: row.auth.clone(),
        argv: row.argv.clone(),
        replay_role: row.replay_role.clone(),
        phase: expected.phase,
        code: expected.code,
        exit: expected.exit,
        wire_frames,
        stdout,
        stderr,
        wire,
    })
}

fn is_http(record: &WireRecord) -> bool {
    is_http_record(&record.bytes)
}

/// Absolute path to the committed pack.
pub fn pack_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("cli-read")
}
