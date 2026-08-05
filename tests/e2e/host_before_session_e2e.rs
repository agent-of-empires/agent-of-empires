//! Full-stack e2e: `host_hooks.before_session` mints environment for a
//! non-sandboxed STRUCTURED worker, and a minted value beats a same-keyed
//! static `environment` entry.
//!
//! `before_start` only fires when a sandbox container comes up, so a host
//! session had no way to compute its agent environment at spawn time; the
//! static `environment` list was the only channel, and it is fixed in config.
//! `before_session` runs a command per host launch and applies its
//! `KEY=VALUE` stdout to the agent, so an account/provider switcher can decide
//! at launch rather than being hardcoded.
//!
//! The proof is read off the adapter's own environment, captured by the shim
//! immediately before it execs the fake agent: downstream of the daemon's
//! `env_clear` + allowlist AND downstream of the detached runner, so it
//! reflects what a real structured worker starts with.
//!
//! Discrimination, three ways. For `CODEX_HOME` there are three candidate
//! values in play and only the minted one may win:
//!
//! 1. the daemon's ambient value (wrong), so a fix that merely widens
//!    `ALWAYS_FORWARD_ENV` stays RED;
//! 2. a static `environment` entry for the SAME key (wrong), so a
//!    regression that applies minted pairs BEFORE the static list, the way
//!    `before_start` does for the first-wins container list, stays RED. The
//!    two precedence rules are deliberately opposite and this is what pins it;
//!    and
//! 3. the minted value (expected).
//!
//! `GIT_CONFIG_GLOBAL` is minted with no static entry and no ambient value, so
//! it doubles as the oracle for *which* shim invocation was the structured
//! worker: only a process that ran the mint can carry it. The `CODEX_HOME`
//! comparison is then a real assertion on that invocation rather than a wait
//! for the value it is about to check.
//!
//! `AOE_TOKEN` is printed by the hook on purpose and must still be refused:
//! `before_session` is trusted enough to set HOME or PATH, but never aoe's own
//! auth token.
//!
//! Compiled only with the `serve` feature (structured view and
//! `aoe add --structured-view` do not exist otherwise). Run via:
//!
//! ```sh
//! cargo test --features serve,e2e-tests --test e2e -- host_before_session
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
/// On timeout, panics listing every value of `key` that WAS observed; for
/// this test that is the interesting half of the failure, because it says
/// whether the losing value came from the daemon's ambient env or from the
/// static `environment` entry.
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
fn before_session_mints_environment_for_structured_worker() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: the worker binds a unix socket under the app dir, and a
    // deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("before_session_env");
    // Tear the worker + daemon down on Drop so a panicking assertion cannot
    // leak a daemon onto the test port between serial tests.
    h.stop_daemon_on_drop();

    let minted_codex_home = h.home_path().join("minted-codex-home");
    let minted_git_config = h.home_path().join("minted-gitconfig");
    // Same key as the mint, different value, declared in the static list.
    let static_codex_home = h.home_path().join("static-codex-home");

    // The daemon's ambient value is deliberately wrong too, so forwarding the
    // daemon's own environment cannot satisfy the assertion either.
    h.set_env(
        "CODEX_HOME",
        &h.home_path()
            .join("wrong-ambient-codex-home")
            .display()
            .to_string(),
    );

    // Top-level `environment` must precede the seeded tables; `[host_hooks]`
    // is appended as its own table after them.
    let config_path = app_dir_in(h.home_path()).join("config.toml");
    let seeded = std::fs::read_to_string(&config_path).expect("read seeded config");
    std::fs::write(
        &config_path,
        format!(
            "environment = [\n  \"CODEX_HOME={static_codex}\",\n]\n\n\
             {seeded}\n\n\
             [host_hooks]\n\
             before_session = [\n  \
               \"echo CODEX_HOME={minted_codex}\",\n  \
               \"echo GIT_CONFIG_GLOBAL={minted_git}\",\n  \
               \"echo AOE_TOKEN=must-not-reach-agent\",\n\
             ]\n",
            static_codex = static_codex_home.display(),
            minted_codex = minted_codex_home.display(),
            minted_git = minted_git_config.display(),
        ),
    )
    .expect("write before_session config");

    let fake_script = h.home_path().join("before-session-script.json");
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
        "BeforeSession",
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

    // Identify the structured worker's invocation by a key ONLY the hook can
    // produce: `GIT_CONFIG_GLOBAL` has no static entry and no ambient value, so
    // a capture carrying it is necessarily one that ran the mint. Waiting on
    // this rather than on `CODEX_HOME` keeps the precedence check below a real
    // assertion: if it waited for the value it is about to assert, that
    // assertion could never fail.
    let capture = wait_for_capture_with(
        &capture_dir,
        "GIT_CONFIG_GLOBAL",
        minted_git_config.to_str().unwrap(),
        // Must outlast the runner socket timeout the harness sets
        // (`AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS=60000`, harness.rs:462), otherwise this can give up
        // before the tolerance it configured is exhausted. A little past it, not exactly equal to
        // it: the capture is written after the runner connects, so an equal budget leaves no slack.
        Duration::from_secs(75),
    );

    // Now the three-way discrimination, on the invocation known to have minted.
    // A wrong value here names its own cause: the static path means precedence
    // is backwards, the ambient path means the mint never reached the adapter.
    assert_eq!(
        env_value(&capture, "CODEX_HOME").as_deref(),
        Some(minted_codex_home.to_str().unwrap()),
        "minted CODEX_HOME must win.\n  \
         static `environment` entry (precedence backwards if seen): {}\n  \
         daemon ambient value (mint lost entirely if seen):         {}",
        static_codex_home.display(),
        h.home_path().join("wrong-ambient-codex-home").display(),
    );

    // No invocation may carry aoe's auth token, however it was produced.
    for (path, capture) in captures(&capture_dir) {
        assert_eq!(
            env_value(&capture, "AOE_TOKEN"),
            None,
            "AOE_TOKEN must never reach the adapter, even when a hook prints it (capture {})",
            path.display()
        );
    }
}
