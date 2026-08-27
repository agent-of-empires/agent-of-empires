//! Full-stack e2e: the configured Host Environment (`Config.environment`)
//! reaches a non-sandboxed STRUCTURED worker, not just a terminal pane.
//!
//! Terminal view installs the resolved entries through the protected pane
//! environment channel (`create_with_size_env`), so `CODEX_HOME=...` reaches the
//! agent a tmux row runs without ever entering the pane command's argv. The
//! structured path builds its own `SpawnConfig`, and before the fix
//! it dropped those entries entirely: a Codex structured worker launched with
//! no `CODEX_HOME`, fell back to a directory it could not write, and died
//! during startup.
//!
//! The proof is read off the adapter's own environment, captured by the shim
//! immediately before it execs the fake agent. That is downstream of the
//! daemon's `env_clear` + allowlist AND downstream of the detached runner, so
//! it reflects what a real structured worker starts with rather than what a
//! half-built `Command` contains.
//!
//! Discrimination: the daemon is started with deliberately WRONG ambient
//! values for both keys and AoE's `environment` carries the expected ones. A
//! fix that merely adds `CODEX_HOME` to the `ALWAYS_FORWARD_ENV` allowlist
//! forwards the daemon's wrong value and therefore stays RED. `AOE_TOKEN` is
//! configured too and must still be refused: `environment` is trusted enough
//! to set HOME or PATH, but never aoe's own auth token.
//!
//! The same capture also carries the automatic desktop/session layer
//! (`DISPLAY`, `XDG_*`), which #3079 wired into the tmux paths only and #3262
//! reopened for the structured view. It is asserted here rather than in its own
//! live-daemon test so the coverage costs no extra daemon spawn.
//!
//! Compiled only with the `serve` feature (structured view and
//! `aoe add --structured-view` do not exist otherwise). Run via:
//!
//! ```sh
//! cargo test --features serve,e2e-tests --test e2e -- acp_host_environment
//! ```
#![cfg(feature = "serve")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{
    app_dir_in, pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness,
};

/// No turns: the worker only has to reach the point of spawning its adapter,
/// which is where the environment is observed.
const EMPTY_SCRIPT: &str = r#"{"turns":[]}"#;

/// Value of `key` in one `env | sort` capture, if present.
fn env_value(capture: &str, key: &str) -> Option<String> {
    capture
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .map(str::to_owned)
}

/// Every per-pid capture written so far, as `(path, contents)`.
fn captures(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            std::fs::read_to_string(&path).ok().map(|c| (path, c))
        })
        .collect()
}

/// Poll until some adapter invocation started with `key=expected`, and return
/// that capture. Any extra shim invocation (tool detection, a capability
/// probe) writes its own file and is simply skipped, so the oracle cannot be
/// stolen by a process that is not the structured worker.
///
/// On timeout, panics listing every value of `key` that WAS observed -- which
/// is the failure a missing forward produces (the daemon's ambient value).
fn wait_for_capture_with(dir: &Path, key: &str, expected: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let seen = captures(dir);
        if let Some((_, capture)) = seen
            .iter()
            .find(|(_, c)| env_value(c, key).as_deref() == Some(expected))
        {
            return capture.clone();
        }
        if Instant::now() >= deadline {
            let observed: Vec<String> = seen
                .iter()
                .map(|(path, c)| {
                    format!(
                        "{}: {key}={}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        env_value(c, key).unwrap_or_else(|| "<unset>".to_string())
                    )
                })
                .collect();
            panic!(
                "no adapter invocation started with {key}={expected} within {timeout:?}.\n\
                 {} capture(s) observed:\n  {}",
                seen.len(),
                observed.join("\n  "),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[parallel]
fn configured_host_environment_reaches_structured_worker() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: the worker binds a unix socket under the app dir, and a
    // deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_host_env");
    // Tear the worker + daemon down on Drop so a panicking assertion cannot
    // leak a daemon onto the test port between serial tests.
    h.stop_daemon_on_drop();

    let expected_codex_home = h.home_path().join("configured-codex-home");
    let expected_git_config = h.home_path().join("configured-gitconfig");

    // The daemon's ambient values are deliberately wrong, so forwarding the
    // daemon's own environment cannot satisfy the assertions below.
    h.set_env(
        "CODEX_HOME",
        &h.home_path()
            .join("wrong-ambient-codex-home")
            .display()
            .to_string(),
    );
    h.set_env(
        "GIT_CONFIG_GLOBAL",
        &h.home_path()
            .join("wrong-ambient-gitconfig")
            .display()
            .to_string(),
    );
    // The desktop/session env the daemon holds must ride along too (#3262):
    // #3079 wired this into the tmux paths only, so a structured worker still
    // started with no `DISPLAY`. Asserted on the same capture this test
    // already waits for, so the coverage costs no extra daemon spawn.
    h.set_env("DISPLAY", ":42");
    h.set_env("XDG_RUNTIME_DIR", "/run/user/4242");
    // The counter-case for the same default posture: an ordinary operator var
    // stays out until `session.inherit_host_environment` is turned on.
    h.set_env("GOPATH", "/scratch/gopath");

    // Global `environment` (a top-level key, above the seeded tables).
    // `AOE_TOKEN` is planted here on purpose: it must be refused.
    let config_path = app_dir_in(h.home_path()).join("config.toml");
    let seeded = std::fs::read_to_string(&config_path).expect("read seeded config");
    std::fs::write(
        &config_path,
        format!(
            "environment = [\n  \"CODEX_HOME={}\",\n  \"GIT_CONFIG_GLOBAL={}\",\n  \"AOE_TOKEN=must-not-reach-agent\",\n]\n\n{seeded}",
            expected_codex_home.display(),
            expected_git_config.display(),
        ),
    )
    .expect("write host environment config");

    let fake_script = h.home_path().join("host-env-script.json");
    std::fs::write(&fake_script, EMPTY_SCRIPT).expect("write fake-acp script");
    let capture_dir = h.home_path().join("adapter-env");
    h.install_acp_shim_capturing_env(&fake_script, &capture_dir);

    // A structured view session needs a git repo as its workspace.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    // The reconciler auto-spawns the structured worker, which spawns the
    // adapter; no prompt is needed to observe its environment.
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "HostEnv",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );

    let capture = wait_for_capture_with(
        &capture_dir,
        "CODEX_HOME",
        expected_codex_home.to_str().unwrap(),
        // Must outlast the runner socket timeout the harness sets
        // (`AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS=60000`, harness.rs:462), otherwise this can give up
        // before the tolerance it configured is exhausted. A little past it, not exactly equal to
        // it: the capture is written after the runner connects, so an equal budget leaves no slack.
        Duration::from_secs(75),
    );

    // Same capture, second key: proves the forward is generic to the
    // configured list and not a special case for one Codex variable.
    assert_eq!(
        env_value(&capture, "GIT_CONFIG_GLOBAL").as_deref(),
        Some(expected_git_config.to_str().unwrap()),
        "configured GIT_CONFIG_GLOBAL must reach the adapter too"
    );

    // Same capture, the desktop/session layer: the daemon's `DISPLAY` and
    // `XDG_*` reach the adapter, downstream of `env_clear` and the runner hop
    // (#3262). `GOPATH` stays out with `inherit_host_environment` off.
    for (key, expected) in [("DISPLAY", ":42"), ("XDG_RUNTIME_DIR", "/run/user/4242")] {
        assert_eq!(
            env_value(&capture, key).as_deref(),
            Some(expected),
            "{key} must reach the adapter (#3262)"
        );
    }
    assert_eq!(
        env_value(&capture, "GOPATH"),
        None,
        "an ordinary operator var must stay out while inherit_host_environment is off"
    );

    // No invocation may carry aoe's auth token, however it was configured.
    for (path, capture) in captures(&capture_dir) {
        assert_eq!(
            env_value(&capture, "AOE_TOKEN"),
            None,
            "AOE_TOKEN must never reach the adapter (capture {})",
            path.display()
        );
    }
}
