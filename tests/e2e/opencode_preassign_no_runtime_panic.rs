//! Regression test for the opencode preassign nested-runtime panic.
//!
//! `preassign_opencode_session_id_impl` (src/session/capture/mod.rs) builds a
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
//! the caller's context. This test enables the opt-in setting, launches a real
//! host OpenCode session through the AoE binary with a fake executable on PATH,
//! and proves preassignment ran without triggering the nested-runtime panic.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

const TITLE: &str = "OpencodePreassignE2E";
const RUNTIME_PANIC: &str = "Cannot start a runtime from within a runtime";

/// Install a fake OpenCode on PATH that records its argv and then idles.
/// The fake server never binds its port, so opt-in preassignment times out and
/// the launch proceeds without a guessed id.
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

fn enable_preassign(h: &TuiTestHarness) {
    let config_path = crate::harness::app_dir_in(h.home_path()).join("config.toml");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&config_path)
        .unwrap_or_else(|error| panic!("failed to open {}: {error}", config_path.display()));
    file.write_all(b"\n[session]\nopencode_preassign_session_id = true\n")
        .expect("enable OpenCode preassignment");
}

fn normal_launch_after_serve(invocations: &str) -> Option<&str> {
    let mut lines = invocations.lines();
    lines.find(|line| line.split_whitespace().next() == Some("serve"))?;
    lines.find(|line| line.split_whitespace().next() != Some("serve"))
}

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", TITLE]);
    }
}

/// Opt-in host preassignment must not panic with "Cannot start a runtime from
/// within a runtime".
#[test]
#[parallel]
fn opencode_opt_in_preassign_does_not_panic_nested_runtime() {
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

    // aoe session start reaches the opt-in host preassign inside the Tokio CLI
    // runtime. The fake server never becomes ready, so AoE launches without a
    // native id instead of guessing one from the store.
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

    // Prove both sides of the timeout contract: preassignment ran, then the
    // real launch started fresh without a guessed native ID.
    let deadline = Instant::now() + Duration::from_secs(5);
    let invocations = loop {
        let invocations = fs::read_to_string(&log_path).unwrap_or_default();
        if normal_launch_after_serve(&invocations).is_some() {
            break invocations;
        }
        assert!(
            Instant::now() < deadline,
            "expected preassign and normal OpenCode invocations; fake log:\n{invocations}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        invocations
            .lines()
            .any(|line| line.split_whitespace().next() == Some("serve")),
        "preassign never spawned `opencode serve`; fake log:\n{invocations}"
    );
    let launch = normal_launch_after_serve(&invocations)
        .expect("normal OpenCode launch after preassignment was not recorded");
    assert!(
        !launch.split_whitespace().any(|arg| arg == "--session"),
        "preassignment timeout must start fresh; fake launch argv: {launch:?}"
    );
}
