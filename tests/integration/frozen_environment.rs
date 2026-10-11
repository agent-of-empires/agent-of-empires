use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use serial_test::parallel;

fn run_payload(directory: &Path, payload: Value, helper: &str) -> Output {
    let descriptor = directory.join("payload.json");
    std::fs::write(&descriptor, payload.to_string()).unwrap();
    Command::new("/bin/sh")
        .args(["-c", "exec \"$1\" \"$3\" 4<\"$2\"", "native-launch"])
        .arg(env!("CARGO_BIN_EXE_aoe"))
        .arg(descriptor)
        .arg(helper)
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
            "__frozen-env",
        );
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{stderr}");
        assert_eq!(result.stdout, b"", "a refused launch must not run env");
        assert!(!stderr.contains("secret-marker"), "{stderr}");
    }
}

#[test]
#[parallel]
fn argv_is_literal_and_explicit_assignments_override_the_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("sh=shim");
    std::os::unix::fs::symlink("/bin/sh", &program).unwrap();
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
        program.to_str().unwrap(),
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
        "__frozen-env",
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

#[test]
#[parallel]
fn post_login_yolo_uses_the_effective_environment_and_literal_argv() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("opencode=shim");
    std::fs::write(
        &program,
        r#"#!/bin/sh
if [ "$1" = --help ]; then
    printf '%s\n' "$DIALECT_HELP"
    printf '%s\n' "$DIALECT_STDERR" >&2
    exit 0
fi
test ! -e /dev/fd/4 || exit 9
case "$DIALECT_HELP$DIALECT_STDERR" in
  *--fork*) test "$OPENCODE_PERMISSION" = '{"*":"allow"}' || exit 10;;
esac
printf '%s\0' "$OPENCODE_PERMISSION" "$EXPLICIT_KEY" "$AMBIENT_FROZEN_SECRET" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sentinel = directory.path().join("must-not-exist");
    let substitution = format!("$(touch {})", sentinel.display());
    let literals = [
        "with spaces",
        "quote'\"",
        "*",
        "$HOME",
        &substitution,
        "A=argument",
        "",
        "line1\nline2",
    ];
    for (help, stderr, legacy, current) in [
        ("--auto", "", false, true),
        ("--fork", "", true, false),
        ("notice", "--fork", true, false),
        ("", "", false, false),
    ] {
        let mut argv = vec![
            "EXPLICIT_KEY=command wins".to_owned(),
            program.to_str().unwrap().to_owned(),
        ];
        argv.extend(literals.iter().map(|value| value.to_string()));
        let output = run_payload(
            directory.path(),
            json!({
                "argv": argv, "cwd": directory.path(),
                "environment": [["DIALECT_HELP", help], ["DIALECT_STDERR", stderr], ["OPENCODE_PERMISSION", "user-policy"], ["EXPLICIT_KEY", "profile"]],
            }),
            "__opencode-yolo",
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected = vec![
            if legacy {
                r#"{"*":"allow"}"#
            } else {
                "user-policy"
            },
            "command wins",
            "ambient-secret-marker",
        ];
        expected.extend(literals);
        if current {
            expected.push("--auto");
        }
        assert_eq!(
            output.stdout,
            format!("{}\0", expected.join("\0")).as_bytes()
        );
        assert!(!sentinel.exists());
    }
}
