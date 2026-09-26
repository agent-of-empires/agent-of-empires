//! Contract Pack governance: the committed `tests/fixtures/cli-read` pack must
//! pass verification, and every kind of drift must be caught before replay.

use std::fs;
use std::path::{Path, PathBuf};

use serial_test::parallel;

use agent_of_empires::cli::runtime_read::pack::{self, pack_root};

fn committed_pack() -> pack::VerifiedPack {
    pack::verify(&pack_root()).expect("the committed Contract Pack verifies")
}

#[test]
#[parallel]
fn the_committed_pack_verifies() {
    let verified = committed_pack();
    assert!(!verified.cases.is_empty());
    for case in &verified.cases {
        assert!(!case.case_id.is_empty());
    }
}

/// A copy of the pack in a temp dir, so a mutation can be made without ever
/// touching the committed fixtures.
fn staged_pack() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp pack root");
    let root = dir.path().join("cli-read");
    copy_tree(&pack_root(), &root);
    (dir, root)
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create pack dir");
    for entry in fs::read_dir(from).expect("read pack dir") {
        let entry = entry.expect("pack entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy pack file");
        }
    }
}

fn rewrite_manifest(root: &Path) {
    let manifest = root.join(pack::MANIFEST_NAME);
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
    fs::write(&manifest, text).expect("rewrite manifest");
}

/// Bring the pack's *self-consistent* bookkeeping back up to date after a
/// content mutation: refresh every `FileRef` against the bytes on disk, drop
/// references to files that were removed, then re-hash the manifest. A test
/// that uses this and still fails is failing on the gate it was aimed at.
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

fn case_dir(root: &Path, case_id: &str) -> PathBuf {
    root.join("cases").join(case_id)
}

/// Editing a manifest-listed fixture must fail the digest check, before any
/// replay could observe the change.
#[test]
#[parallel]
fn a_tampered_fixture_fails_the_manifest_digest() {
    let (_dir, root) = staged_pack();
    let stdout = case_dir(&root, "uds-list-nominal").join("expected.stdout");
    let mut bytes = fs::read(&stdout).expect("read golden");
    bytes.push(b'x');
    fs::write(&stdout, bytes).expect("tamper");

    let error = pack::verify(&root).expect_err("tampered fixture is rejected");
    assert!(
        error
            .to_string()
            .contains("does not match its manifest digest"),
        "unexpected error: {error}"
    );
}

/// A file dropped into the pack without a manifest entry is drift too.
#[test]
#[parallel]
fn an_unlisted_extra_file_fails_the_universe_check() {
    let (_dir, root) = staged_pack();
    fs::write(root.join("cases/uds-list-nominal/notes.txt"), b"stray").expect("write stray file");

    let error = pack::verify(&root).expect_err("unlisted file is rejected");
    assert!(
        error.to_string().contains("manifest universe mismatch"),
        "unexpected error: {error}"
    );
}

/// The manifest is the universe: dropping a line for a real file must fail even
/// though the remaining digests are all correct.
#[test]
#[parallel]
fn a_shortened_manifest_fails_the_universe_check() {
    let (_dir, root) = staged_pack();
    let manifest = root.join(pack::MANIFEST_NAME);
    let text = fs::read_to_string(&manifest).expect("read manifest");
    let kept: String = text
        .lines()
        .filter(|line| !line.ends_with("  cases.schema.json"))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&manifest, kept).expect("rewrite manifest");

    let error = pack::verify(&root).expect_err("short manifest is rejected");
    assert!(
        error.to_string().contains("manifest universe mismatch"),
        "unexpected error: {error}"
    );
}

/// Re-serialized `CASES.json` with different key order is drift even though it
/// parses to the same value: the index is canonical by contract.
#[test]
#[parallel]
fn a_non_canonical_cases_index_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = root.join(pack::CASES_NAME);
    let mut bytes = fs::read(&path).expect("read CASES.json");
    // One trailing byte: still valid JSON, same value, not canonical bytes.
    bytes.push(b'\n');
    fs::write(&path, bytes).expect("rewrite CASES.json");
    rewrite_manifest(&root);

    let error = pack::verify(&root).expect_err("non-canonical CASES.json is rejected");
    assert!(
        error.to_string().contains("is not RFC 8785 canonical"),
        "unexpected error: {error}"
    );
}

/// Editing a declared digest without touching the file must fail.
#[test]
#[parallel]
fn a_wrong_declared_digest_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = root.join(pack::CASES_NAME);
    let text = fs::read_to_string(&path).expect("read CASES.json");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("parse CASES.json");
    let stale = serde_json::json!("0".repeat(64));
    let case = &mut value["cases"][0];
    let target = case["all_files"][0]["path"].clone();
    case["all_files"][0]["sha256"] = stale.clone();
    for output in case["output_files"].as_array_mut().expect("output refs") {
        if output["path"] == target {
            output["sha256"] = stale.clone();
        }
    }
    fs::write(&path, canonical(&value).as_bytes()).expect("rewrite");
    rewrite_manifest(&root);

    let error = pack::verify(&root).expect_err("wrong declared digest is rejected");
    assert!(
        error.to_string().contains("FileRef"),
        "unexpected error: {error}"
    );
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

/// The golden stdout is a digest over real bytes, so editing it without
/// updating `expected.json` must fail.
#[test]
#[parallel]
fn a_changed_golden_without_its_expected_metadata_is_rejected() {
    let (_dir, root) = staged_pack();
    let stdout = case_dir(&root, "http-loopback-status-nominal").join("expected.stdout");
    fs::write(&stdout, b"{}\n").expect("tamper");
    restage(&root);

    let error = pack::verify(&root).expect_err("changed golden is rejected");
    assert!(
        error.to_string().contains("hashes do not match"),
        "unexpected error: {error}"
    );
}

/// The phase/code/exit matrix is closed: an exit that the pair does not allow
/// is drift even with every digest correct.
#[test]
#[parallel]
fn an_exit_outside_the_phase_code_matrix_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = case_dir(&root, "http-loopback-unauthorized").join("expected.json");
    let text = fs::read_to_string(&path).expect("read expected.json");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("parse expected.json");
    value["exit"] = serde_json::json!(2);
    fs::write(&path, canonical(&value).as_bytes()).expect("rewrite");
    restage(&root);

    let error = pack::verify(&root).expect_err("bad exit is rejected");
    assert!(
        error
            .to_string()
            .contains("exits 2 but its phase/code pair allows [4]"),
        "unexpected error: {error}"
    );
}

/// `error.json` exists exactly when the expected exit is nonzero.
#[test]
#[parallel]
fn a_failure_case_without_error_json_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = case_dir(&root, "https-unauthorized").join("error.json");
    fs::remove_file(&path).expect("remove error.json");
    restage(&root);

    let error = pack::verify(&root).expect_err("missing error.json is rejected");
    assert!(
        error.to_string().contains("has no error.json"),
        "unexpected error: {error}"
    );
}

/// The error message is byte-identical to the stderr it describes.
#[test]
#[parallel]
fn an_error_message_that_disagrees_with_stderr_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = case_dir(&root, "http-loopback-forbidden").join("error.json");
    let text = fs::read_to_string(&path).expect("read error.json");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("parse error.json");
    value["message"] = serde_json::json!("daemon read: something_else\n");
    fs::write(&path, canonical(&value).as_bytes()).expect("rewrite");
    restage(&root);

    let error = pack::verify(&root).expect_err("mismatched error message is rejected");
    assert!(
        error
            .to_string()
            .contains("byte-identical to expected.stderr"),
        "unexpected error: {error}"
    );
}

/// An extra file inside a case directory breaks the exact per-case layout.
#[test]
#[parallel]
fn an_extra_case_file_breaks_the_per_case_layout() {
    let (_dir, root) = staged_pack();
    fs::write(
        case_dir(&root, "uds-list-nominal").join("notes.txt"),
        b"stray",
    )
    .expect("write");
    rewrite_manifest(&root);

    let error = pack::verify(&root).expect_err("extra case file is rejected");
    assert!(
        error.to_string().contains("layout is"),
        "unexpected error: {error}"
    );
}

/// A case directory named with a path separator in its id is refused before any
/// per-case path is constructed.
#[test]
#[parallel]
fn a_case_id_that_is_not_one_component_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = root.join(pack::CASES_NAME);
    let text = fs::read_to_string(&path).expect("read CASES.json");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("parse CASES.json");
    value["cases"][0]["case_id"] = serde_json::json!("../escape");
    fs::write(&path, canonical(&value).as_bytes()).expect("rewrite");
    restage(&root);

    let error = pack::verify(&root).expect_err("traversing case_id is rejected");
    assert!(
        error.to_string().contains("not a single valid component"),
        "unexpected error: {error}"
    );
}

/// The recorded wire transcript must agree with the frame count its metadata
/// claims, so a dropped or invented frame is caught statically.
#[test]
#[parallel]
fn a_wire_transcript_that_loses_a_frame_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = case_dir(&root, "http-loopback-status-nominal").join("wire.raw");
    let bytes = fs::read(&path).expect("read wire.raw");
    // Drop the final record: the Snapshot the client renders.
    // Keep only the HTTP request and the 101 response: the two application
    // frames the client renders are gone.
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(
            bytes[offset + 2..offset + 6]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        bounds.push((offset, offset + 6 + length));
        offset += 6 + length;
    }
    assert!(
        bounds.len() > 2,
        "transcript has application frames to drop"
    );
    let kept: Vec<u8> = bounds[..2]
        .iter()
        .flat_map(|(start, end)| bytes[*start..*end].to_vec())
        .collect();
    fs::write(&path, kept).expect("truncate wire.raw");
    restage(&root);

    let error = pack::verify(&root).expect_err("short transcript is rejected");
    assert!(
        error.to_string().contains("wire frames"),
        "unexpected error: {error}"
    );
}

/// A case must not point at a command/alias pair the matrix forbids.
#[test]
#[parallel]
fn a_command_alias_pair_outside_the_matrix_is_rejected() {
    let (_dir, root) = staged_pack();
    let path = root.join(pack::CASES_NAME);
    let text = fs::read_to_string(&path).expect("read CASES.json");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("parse CASES.json");
    value["cases"][0]["command"] = serde_json::json!("status");
    fs::write(&path, canonical(&value).as_bytes()).expect("rewrite");
    restage(&root);

    let error = pack::verify(&root).expect_err("bad command/alias pair is rejected");
    assert!(
        error
            .to_string()
            .contains("command/alias outside the matrix"),
        "unexpected error: {error}"
    );
}

/// The pack is unsigned integrity governance: a directory link is refused
/// rather than followed, so the universe is always the real file set.
#[test]
#[parallel]
fn a_symlinked_case_directory_is_refused() {
    let (_dir, root) = staged_pack();
    let real = root.join("escape");
    fs::create_dir_all(&real).expect("create escape dir");
    fs::write(real.join("wire.raw"), b"x").expect("write");
    let link = case_dir(&root, "linked-case");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).expect("symlink case dir");

    let error = pack::verify(&root).expect_err("symlinked entry is refused");
    assert!(
        error.to_string().contains("non-regular entry"),
        "unexpected error: {error}"
    );
}

/// A symlinked file inside a case is refused for the same reason.
#[test]
#[parallel]
fn a_symlinked_case_file_is_refused() {
    let (_dir, root) = staged_pack();
    let real = root.join("escape.raw");
    fs::write(&real, b"x").expect("write");
    let case = case_dir(&root, "uds-list-nominal");
    let linked = case.join("expected.stdout");
    fs::rename(&linked, case.join("expected.stdout.real")).expect("move golden aside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &linked).expect("symlink golden");

    let error = pack::verify(&root).expect_err("symlinked entry is refused");
    assert!(
        error.to_string().contains("non-regular entry"),
        "unexpected error: {error}"
    );
}
