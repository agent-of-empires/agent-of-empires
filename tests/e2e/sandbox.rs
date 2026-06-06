use serial_test::serial;

use crate::harness::TuiTestHarness;

/// Exercises `aoe add --sandbox` which builds the full container config.
/// This would have caught the duplicate mount points bug (commit 92d2e53).
///
/// Requires a running Docker daemon; marked `#[ignore]` for CI.
#[test]
#[serial]
#[ignore = "requires Docker daemon"]
fn test_cli_add_with_sandbox() {
    let h = TuiTestHarness::new("cli_sandbox");
    let project = h.project_path();

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "Sandbox E2E",
        "--sandbox",
    ]);
    assert!(
        output.status.success(),
        "aoe add --sandbox failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let list_output = h.run_cli(&["list", "--json"]);
    assert!(list_output.status.success());

    let stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        stdout.contains("Sandbox E2E"),
        "list should contain the sandboxed session.\nOutput:\n{}",
        stdout
    );
}

/// Regression test for #1989: on_create hooks must execute inside the sandbox
/// container, not on the host, when `aoe add --sandbox` is used.
///
/// Before the fix, the CLI called `execute_hooks()` unconditionally with
/// `HookTarget::Local`, ignoring `sandbox_info`. The TUI already handled this
/// correctly via `execute_hooks_in_container_streamed`.
///
/// Requires a running Docker daemon; marked `#[ignore]` for CI.
#[test]
#[serial]
#[ignore = "requires Docker daemon"]
fn test_cli_add_sandbox_on_create_hooks_run_in_container() {
    let h = TuiTestHarness::new("cli_sandbox_hooks");
    let project = h.project_path();

    // A path in /tmp that both host and container own as separate namespaces.
    // If the hook runs on the HOST (the bug), this file appears on the host.
    // If it runs correctly INSIDE the container, it only exists in the
    // container's ephemeral /tmp and is invisible on the host.
    let marker = format!("/tmp/aoe-sandbox-hook-{}", std::process::id());
    // Guard against a stale marker from a prior crashed run.
    let _ = std::fs::remove_file(&marker);

    let aoe_config_dir = project.join(".agent-of-empires");
    std::fs::create_dir_all(&aoe_config_dir).expect("create config dir");
    std::fs::write(
        aoe_config_dir.join("config.toml"),
        format!("[hooks]\non_create = [\"touch {}\"]\n", marker),
    )
    .expect("write repo config");

    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--sandbox",
        "--trust-hooks",
        "-t",
        "SandboxHookTest",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "aoe add --sandbox --trust-hooks failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("on_create hooks completed"),
        "expected 'on_create hooks completed' in stdout.\nstdout: {}",
        stdout
    );
    // Regression guard: the marker must NOT exist on the host. Before the fix,
    // the CLI ran hooks via HookTarget::Local so the file would have appeared
    // here. The correct path runs the hook inside the container, where the
    // host's /tmp is not visible.
    assert!(
        !std::path::Path::new(&marker).exists(),
        "on_create hook ran on the host instead of inside the container \
         (regression of #1989): marker found at {}",
        marker
    );
}
