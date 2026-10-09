use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use serial_test::parallel;

fn run_payload(directory: &Path, payload: Value) -> Output {
    let descriptor = directory.join("payload.json");
    std::fs::write(&descriptor, payload.to_string()).unwrap();
    Command::new("/bin/sh")
        .args(["-c", "exec \"$1\" __frozen-env 4<\"$2\"", "frozen-env"])
        .arg(env!("CARGO_BIN_EXE_aoe"))
        .arg(descriptor)
        .env("AMBIENT_FROZEN_SECRET", "ambient-secret-marker")
        .output()
        .unwrap()
}

#[test]
#[parallel]
fn a_missing_program_cannot_dump_the_captured_environment() {
    let directory = tempfile::tempdir().unwrap();
    for argv in [
        json!([]),
        json!([""]),
        json!(["OPENCODE_PERMISSION=assignment-secret-marker"]),
        json!(["OPENCODE_PERMISSION=assignment-secret-marker", ""]),
    ] {
        let result = run_payload(
            directory.path(),
            json!({
                "argv": argv,
                "cwd": directory.path(),
                "environment": [["FROZEN_SECRET", "captured-secret-marker"]],
            }),
        );
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{stderr}");
        assert_eq!(result.stdout, b"", "a refused launch must not run env");
        assert!(stderr.contains("frozen launch program"), "{stderr}");
        assert!(!stderr.contains("secret-marker"), "{stderr}");
    }
}

#[test]
#[parallel]
fn argv_is_literal_and_explicit_assignments_override_the_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let sentinel = directory.path().join("must-not-exist");
    let substitution = format!("$(touch {})", sentinel.display());
    let literals = [
        "with spaces",
        "single'quote",
        "double\"quote",
        "back\\slash",
        "$HOME",
        "$$",
        &substitution,
        "*",
        ";",
        "A=argument",
        "",
    ];
    let mut argv = vec![
        r#"OPENCODE_PERMISSION={"*":"allow"}"#,
        "A=with spaces='\"$*=tail",
        "/bin/sh",
        "-c",
        "printf '%s\\0' \"$OPENCODE_PERMISSION\" \"$A\" \"${AMBIENT_FROZEN_SECRET-unset}\" \"$@\"; test ! -e /dev/fd/4",
        "probe",
    ];
    argv.extend(literals);
    let result = run_payload(
        directory.path(),
        json!({
            "argv": argv,
            "cwd": directory.path(),
            "environment": [["OPENCODE_PERMISSION", "deny"], ["A", "snapshot"]],
        }),
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut expected = vec![r#"{"*":"allow"}"#, "with spaces='\"$*=tail", "unset"];
    expected.extend(literals);
    assert_eq!(
        result.stdout,
        format!("{}\0", expected.join("\0")).as_bytes()
    );
    assert!(!sentinel.exists(), "argv must never invoke shell expansion");
}
