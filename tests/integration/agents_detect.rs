//! `aoe agents detect`: the hidden fixture command that runs a detection
//! manifest over a saved pane. Exit 0 a rule matched, 1 none did, 2 bad input.

use std::path::Path;
use std::process::Command;

const MANIFEST: &str = r#"
id = "fixture"
[[rules]]
id = "working"
state = "running"
priority = 500
region = "whole_recent"
visible = true
contains = ["working"]
"#;

fn run(home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["agents", "detect"])
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("AOE_TMUX_SOCKET", home.join("tmux.sock"))
        .env_remove("AGENT_OF_EMPIRES_PROFILE")
        .output()
        .expect("run aoe");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn agents_detect_reports_the_deciding_rule_and_exit_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let write = |name: &str, content: &str| {
        let path = home.join(name);
        std::fs::write(&path, content).expect("write fixture");
        path.to_string_lossy().into_owned()
    };
    let manifest = write("manifest.toml", MANIFEST);
    let busy = write("busy.txt", "\x1b[1mWorking\x1b[0m on it\n");
    let quiet = write("quiet.txt", "nothing to see\n");
    let broken = write("broken.toml", "not = [valid");
    let hook = write("hook", "waiting\n");

    let (code, stdout, _) = run(home, &["--manifest", &manifest, "--pane", &busy, "--json"]);
    assert_eq!(code, Some(0));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(
        json,
        serde_json::json!({
            "matched": true,
            "rule": "working",
            "status": "running",
            "visible": true,
            "diagnostic": null,
        })
    );

    let (code, stdout, _) = run(home, &["--manifest", &manifest, "--pane", &quiet, "--json"]);
    assert_eq!(code, Some(1));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(json["matched"], false);
    assert_eq!(json["rule"], serde_json::Value::Null);
    assert_eq!(json["status"], serde_json::Value::Null);

    // A hook status alone decides the rule `hook_waiting` shared by every manifest.
    let (code, stdout, _) = run(
        home,
        &[
            "--manifest",
            &manifest,
            "--pane",
            &write("empty.txt", ""),
            "--hook",
            &hook,
            "--json",
        ],
    );
    assert_eq!(code, Some(0), "{stdout}");
    assert!(stdout.contains("\"hook_waiting\""), "{stdout}");

    for args in [
        vec!["--manifest", &broken, "--pane", &busy],
        vec!["--manifest", &manifest, "--pane", "/nonexistent/pane"],
        vec!["--manifest", &manifest, "--pane", &busy, "--hook", &quiet],
    ] {
        let (code, stdout, stderr) = run(home, &args);
        assert_eq!(code, Some(2), "{args:?}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.contains("Error:"), "{args:?}: {stderr}");
    }
}
