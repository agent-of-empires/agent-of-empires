//! Regression test for the opencode preassign nested-runtime panic.
//!
//! `preassign_opencode_session_id_impl` (src/session/capture.rs) builds a
//! current-thread Tokio runtime and `block_on`s an HTTP call to reserve the
//! opencode `ses_` id. The CLI entrypoint is `#[tokio::main]`, so before the
//! fix, launching an opencode session ran that `block_on` inside a live
//! runtime and aborted the process with:
//!
//! ```text
//! Cannot start a runtime from within a runtime.
//! ```
//!
//! Running the preassign on a dedicated OS thread makes it safe regardless of
//! the caller's context. This test enables the opt-in preassign, launches a
//! real opencode session through the `aoe` binary with a fake `opencode` on
//! PATH, and asserts the panic is gone, while proving the preassign path
//! actually ran (the fake logs a `serve` invocation).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

const TITLE: &str = "OpencodePreassignE2E";
const RUNTIME_PANIC: &str = "Cannot start a runtime from within a runtime";

/// Install a fake `opencode` on PATH that records its argv (so the test can
/// prove preassign spawned `opencode serve`) and then idles. The fake `serve`
/// never binds its port, so the preassign readiness probe times out and aoe
/// falls back to the poller: the graceful path, without a real opencode.
fn install_fake_opencode(h: &mut TuiTestHarness) -> PathBuf {
    let bin = h.install_path_command("opencode");
    let log = h.home_path().join("fake-opencode.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec sleep 30\n",
        log.display()
    );
    let script_path = bin.join("opencode");
    fs::write(&script_path, script).expect("write fake opencode");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("chmod fake opencode");
    }
    log
}

/// Turn on the opt-in preassign path in the seeded test config. The harness
/// seeds only `[updates]` and `[app_state]`, so appending a `[session]` table
/// here does not clash.
fn enable_preassign(h: &TuiTestHarness) {
    let config_path = crate::harness::app_dir_in(h.home_path()).join("config.toml");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&config_path)
        .unwrap_or_else(|e| panic!("failed to open {}: {}", config_path.display(), e));
    file.write_all(b"\n[session]\nopencode_preassign_session_id = true\n")
        .expect("enable opencode preassign");
}

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", TITLE]);
    }
}

/// Launching an opencode session with preassign enabled must not panic with
/// "Cannot start a runtime from within a runtime".
#[test]
#[parallel]
fn opencode_launch_with_preassign_does_not_panic_nested_runtime() {
    require_tmux!();

    let mut h = TuiTestHarness::new("opencode_preassign_no_runtime_panic");
    let log_path = install_fake_opencode(&mut h);
    enable_preassign(&h);
    let project = h.project_path();

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "opencode",
        "-t",
        TITLE,
    ]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let _cleanup = StopSessionOnDrop { h: &h };

    // `aoe session start` reaches `acquire_session_id` -> the opencode preassign
    // inside the `#[tokio::main]` CLI runtime. Before the fix this aborted with
    // the nested-runtime panic; after it, the preassign runs on a dedicated
    // thread, the fake serve never gets ready, and aoe falls back to the poller.
    let start = h.run_cli(&["session", "start", TITLE]);
    let stderr = String::from_utf8_lossy(&start.stderr);

    assert!(
        !stderr.contains(RUNTIME_PANIC),
        "opencode launch hit the nested-runtime panic:\n{stderr}"
    );
    assert!(
        start.status.success(),
        "aoe session start failed:\n{stderr}"
    );

    // Prove the preassign path actually ran; otherwise "no panic" would be
    // vacuously true. The fake opencode logs every invocation, and preassign
    // spawns `opencode serve` right before the `block_on` that used to panic.
    let invocations = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        invocations.lines().any(|line| line.contains("serve")),
        "preassign never spawned `opencode serve`; fake log:\n{invocations}"
    );
}
