//! `aoe acp lint-stream` exits 0 on a conformant stream and 1 on findings.

use std::path::PathBuf;
use std::process::Command;

fn lint(fixture: &str) -> (Option<i32>, String) {
    let home = tempfile::tempdir().expect("temp home");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/acp-stream")
        .join(fixture);
    let out = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["acp", "lint-stream", "--require-hello"])
        .arg(path)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .output()
        .expect("run aoe");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn exit_code_follows_the_findings() {
    let (code, stdout) = lint("valid.ndjson");
    assert_eq!((code, stdout.as_str()), (Some(0), ""));

    let (code, stdout) = lint("invalid.ndjson");
    assert_eq!(code, Some(1));
    assert!(stdout.contains("invalid_hello"), "{stdout}");
    assert!(stdout.contains("bad_envelope"), "{stdout}");
}
