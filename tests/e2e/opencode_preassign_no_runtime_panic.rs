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
//! the caller's context. This test launches a real host opencode session through
//! the aoe binary with a fake opencode on PATH and proves automatic
//! preassignment ran without triggering the nested-runtime panic.

use std::fs;
use std::path::PathBuf;

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

const TITLE: &str = "OpencodePreassignE2E";
const RUNTIME_PANIC: &str = "Cannot start a runtime from within a runtime";

/// Install a fake opencode on PATH that records its argv and then idles.
/// The fake serve never binds its port, so automatic preassignment times out
/// and the launch proceeds without a guessed id.
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

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", TITLE]);
    }
}

/// Automatic host preassignment must not panic with "Cannot start a runtime
/// from within a runtime".
#[test]
#[parallel]
fn opencode_automatic_preassign_does_not_panic_nested_runtime() {
    require_tmux!();

    let mut h = TuiTestHarness::new("opencode_preassign_no_runtime_panic");
    let log_path = install_fake_opencode(&mut h);
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

    // aoe session start reaches the automatic host preassign inside the
    // Tokio CLI runtime. The fake server never becomes ready, so AoE launches
    // without a native id instead of guessing one from the store.
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
