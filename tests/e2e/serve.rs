//! E2E coverage for the serve dialog state machine.
//!
//! Targeted regression tests for the `R`-key ModePicker + Confirm flow
//! introduced with the Tailscale Funnel transport picker. Compiled only
//! with `--features serve` since the serve dialog doesn't exist
//! otherwise; run via:
//!
//! ```sh
//! cargo test --test e2e --features serve -- tui_serve_dialog
//! ```
#![cfg(feature = "serve")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{pick_free_port, require_tmux, wait_for_port, TuiTestHarness};

/// Resolve the daemon's PID file inside the harness's isolated home.
/// Mirrors `crate::session::get_app_dir`'s platform split.
fn daemon_pid_path(h: &TuiTestHarness) -> PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("serve.pid")
}

/// Resolve the daemon's persisted launch-state file inside the harness's
/// isolated home.
fn daemon_launch_path(h: &TuiTestHarness) -> PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("serve.launch")
}

/// True iff the kernel still has a process with this PID.
fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Pressing `R` from the home screen opens the serve ModePicker,
/// which must render both cards (Local + Internet) and surface the
/// transport-picker-deferred hint on the Tunnel card ("Pick transport
/// on next screen.").
#[test]
#[serial]
fn tui_serve_dialog_opens_to_mode_picker() {
    require_tmux!();

    let mut h = TuiTestHarness::new("serve_mode_picker");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("R");

    h.wait_for("How should this be reachable?");
    h.assert_screen_contains("Local network");
    h.assert_screen_contains("Internet (HTTPS)");
    // The Tunnel card defers the transport choice to the next screen.
    // If this line disappears, the ModePicker copy is out of sync with
    // the Confirm-screen picker it hands off to.
    h.assert_screen_contains("Pick transport on next screen.");
}

/// Esc dismisses the serve dialog and returns to the home screen
/// without spawning anything. Regression guard against state-transition
/// bugs where ModePicker might latch onto a stale mode.
#[test]
#[serial]
fn tui_serve_dialog_escape_returns_home() {
    require_tmux!();

    let mut h = TuiTestHarness::new("serve_mode_picker_esc");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("R");
    h.wait_for("How should this be reachable?");

    h.send_keys("Escape");
    // Home-screen footer is the tell that we've returned.
    h.wait_for("No sessions yet");
}

/// `aoe serve --daemon` must spawn a child that actually binds the port and
/// stays alive. Regression guard for the self-detection bug where the parent
/// pre-wrote the child's PID into `serve.pid`, then the child re-entered
/// `run()`, found its own PID via `daemon_pid()`, and bailed with
/// "A serve daemon is already running" — about itself.
#[test]
#[serial]
fn cli_serve_daemon_starts_and_stops_cleanly() {
    let h = TuiTestHarness::new("serve_daemon_lifecycle");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    let pid_path = daemon_pid_path(&h);
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {} (child likely self-detected and exited).\n\
         pid file exists: {}\n\
         debug.log:\n{}",
        port,
        pid_path.exists(),
        std::fs::read_to_string(pid_path.with_file_name("debug.log")).unwrap_or_default(),
    );

    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("serve.pid should exist after daemon starts")
        .trim()
        .parse()
        .expect("serve.pid should contain a valid integer");
    assert!(
        pid_alive(pid),
        "child PID {} not alive after port bind",
        pid
    );

    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "aoe serve --stop failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr),
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pid_alive(pid),
        "daemon PID {} still alive after --stop",
        pid
    );
    assert!(
        !pid_path.exists(),
        "serve.pid should be cleaned up after --stop, found at {}",
        pid_path.display()
    );
}

/// `aoe serve --restart` must stop the running daemon and spawn a fresh
/// one from the persisted launch state: a new PID, the old one gone, the
/// same port rebound, and `serve.launch` rewritten. Locks in #1794's
/// restart primitive end to end.
#[test]
#[serial]
fn cli_serve_restart_replays_launch_state() {
    let h = TuiTestHarness::new("serve_restart_replays");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "initial --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let pid_path = daemon_pid_path(&h);
    let launch_path = daemon_launch_path(&h);
    let pid1: i32 = std::fs::read_to_string(&pid_path)
        .expect("serve.pid after start")
        .trim()
        .parse()
        .expect("serve.pid holds an integer");
    assert!(
        launch_path.exists(),
        "serve.launch should be written on daemon start, missing at {}",
        launch_path.display()
    );

    let restart = h.run_cli(&["serve", "--restart"]);
    assert!(
        restart.status.success(),
        "aoe serve --restart failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&restart.stdout),
        String::from_utf8_lossy(&restart.stderr),
    );

    // The replacement child rebinds the same persisted port.
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "restarted daemon never rebound port {port}"
    );

    // serve.pid now names a different, live process; the old one is gone.
    let pid2: i32 = std::fs::read_to_string(&pid_path)
        .expect("serve.pid after restart")
        .trim()
        .parse()
        .expect("serve.pid holds an integer");
    assert_ne!(pid1, pid2, "restart should spawn a new daemon PID");
    assert!(
        pid_alive(pid2),
        "restarted daemon PID {pid2} should be alive"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pid_alive(pid1),
        "old daemon PID {pid1} still alive after restart"
    );
    assert!(
        launch_path.exists(),
        "serve.launch should be rewritten by the restart"
    );

    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "--stop after restart failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr),
    );
}

/// Regression guard for the sink consolidation (issue #1124): the daemon
/// must write its tracing stream to the configured `debug.log`, and the
/// retired `serve.log` must not reappear. Without this guard, a future
/// change that misclassifies the daemon child as `ServeForeground` (or
/// reintroduces the `serve.log` redirect) would slip through CI.
#[test]
#[serial]
fn cli_serve_daemon_writes_marker_to_debug_log_not_serve_log() {
    let h = TuiTestHarness::new("serve_daemon_logging_sinks");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(start.status.success(), "aoe serve --daemon failed");

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    let app_dir = crate::harness::app_dir_in(h.home_path());
    let debug_log = app_dir.join("debug.log");
    let serve_log = app_dir.join("serve.log");

    let debug_contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {}", debug_log.display(), e));
    assert!(
        debug_contents.contains("[AOE_START_MARKER]"),
        "debug.log should carry the filter-immune startup marker; got: {:?}",
        debug_contents
    );
    assert!(
        !serve_log.exists(),
        "serve.log must not be re-created post-consolidation, found at {}",
        serve_log.display()
    );

    let _ = h.run_cli(&["serve", "--stop"]);
}

/// `--auth=passphrase` is the load-bearing new mode: no URL token, the
/// passphrase wall is the only human gate. Exercises the wall end-to-end
/// against a real daemon so the new `run_passphrase_wall` middleware
/// branch is locked in CI rather than relying on manual smoke tests.
///
/// Flow:
///   1. GET `/api/about` from a simulated non-loopback caller (loopback
///      socket + `X-Forwarded-For: 10.0.0.5`, which `resolve_client_ip`
///      trusts because the socket itself is loopback) -> 401
///      `login_required`. Proves the wall still blocks remote API
///      traffic after the #1525 loopback bypass landed.
///   2. POST `/api/login` with the correct passphrase + a fresh
///      device-binding secret -> 200 with `Set-Cookie: aoe_session=`.
///   3. GET `/api/about` carrying the session cookie + binding header
///      (no XFF so the caller is loopback) -> 200, body has
///      `"auth_mode":"passphrase"`. Proves both the wall handoff and
///      the `/api/about` mode-derivation surface.
#[test]
#[serial]
fn cli_serve_auth_passphrase_login_round_trip() {
    let h = TuiTestHarness::new("serve_auth_passphrase");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    // 32 random-ish bytes; the contents don't matter, just the length and encoding.
    let binding_raw: [u8; 32] = [0x5Au8; 32];
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let binding_b64 = URL_SAFE_NO_PAD.encode(binding_raw);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result: Result<(), String> = rt.block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        // reqwest's `cookies` feature is not enabled in the workspace, so
        // pull the session out of `Set-Cookie` by hand. Cheaper than
        // touching Cargo.toml just for one test.
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;

        // 1. Unauthenticated GET from a simulated remote caller must
        //    come back with 401 + login_required body. The daemon
        //    binds loopback, so `resolve_client_ip` trusts XFF; we
        //    pin a non-loopback last-hop IP so the #1525 loopback
        //    bypass does not fire.
        let about_unauth = client
            .get(format!("{base}/api/about"))
            .header("x-forwarded-for", "10.0.0.5")
            .send()
            .await
            .map_err(|e| format!("GET /api/about (unauth): {e}"))?;
        let status = about_unauth.status();
        let body: serde_json::Value = about_unauth
            .json()
            .await
            .map_err(|e| format!("decode unauth body: {e}"))?;
        if status != reqwest::StatusCode::UNAUTHORIZED
            || body.get("error").and_then(|v| v.as_str()) != Some("login_required")
        {
            return Err(format!(
                "expected 401 login_required, got status={status} body={body}"
            ));
        }

        // 2. POST /api/login with matching passphrase + device binding.
        let login = client
            .post(format!("{base}/api/login"))
            .json(&serde_json::json!({
                "passphrase": "e2e-pass",
                "device_binding_secret": binding_b64,
            }))
            .send()
            .await
            .map_err(|e| format!("POST /api/login: {e}"))?;
        if !login.status().is_success() {
            let s = login.status();
            let b = login.text().await.unwrap_or_default();
            return Err(format!("login failed: status={s} body={b}"));
        }

        // Pull `aoe_session=...` out of the first Set-Cookie value that
        // names it. Multiple Set-Cookie headers may come back (login
        // cookie, push-related cookies, etc.); pick the one we need.
        let session_cookie = login
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .find_map(|v| {
                let s = v.to_str().ok()?;
                let first = s.split(';').next()?.trim();
                if first.starts_with("aoe_session=") {
                    Some(first.to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| "login response missing aoe_session Set-Cookie".to_string())?;

        // 3. Authenticated GET must succeed and report auth_mode=passphrase.
        let about = client
            .get(format!("{base}/api/about"))
            .header("cookie", &session_cookie)
            .header("x-aoe-device-binding", &binding_b64)
            .send()
            .await
            .map_err(|e| format!("GET /api/about (auth): {e}"))?;
        if !about.status().is_success() {
            let s = about.status();
            let b = about.text().await.unwrap_or_default();
            return Err(format!(
                "authenticated /api/about failed: status={s} body={b}"
            ));
        }
        let body: serde_json::Value = about
            .json()
            .await
            .map_err(|e| format!("decode about body: {e}"))?;
        match body.get("auth_mode").and_then(|v| v.as_str()) {
            Some("passphrase") => Ok(()),
            other => Err(format!(
                "expected auth_mode=passphrase, got {other:?} in {body}"
            )),
        }
    });

    // Always tear the daemon down before asserting, so a failed assert
    // doesn't leak a process that owns the test port.
    let _ = h.run_cli(&["serve", "--stop"]);

    if let Err(e) = result {
        panic!("{e}");
    }
}

/// Regression test for #1525. With `--auth=passphrase` the daemon used
/// to route loopback callers through the passphrase wall, breaking the
/// local TUI structured view attach: it had no session cookie + device binding
/// to present so `/api/sessions/{id}/structured view/replay` and the structured view ws
/// upgrade always 401'd. The fix mirrors the token-auth path's #1168
/// carve-out and treats loopback as fs-trusted.
///
/// Flow:
///   1. Start `aoe serve --daemon --auth=passphrase`.
///   2. GET `/api/about` from 127.0.0.1 with no session cookie, no
///      device binding, no XFF -> 200 with `"auth_mode":"passphrase"`.
///      Without the bypass this would 401 `login_required`.
///   3. GET `/api/sessions` from 127.0.0.1 -> 200 (proves the bypass
///      covers the structured view REST surface, not just `/api/about`).
#[test]
#[serial]
fn cli_serve_auth_passphrase_loopback_bypass() {
    let h = TuiTestHarness::new("serve_auth_passphrase_loopback");
    let port = pick_free_port();
    let port_s = port.to_string();

    let start = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port_s,
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result: Result<(), String> = rt.block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build client: {e}"))?;

        // No cookie, no device binding, no XFF: the loopback bypass
        // (#1525) lets the request through. Pre-fix this would have
        // returned 401 login_required.
        let about = client
            .get(format!("{base}/api/about"))
            .send()
            .await
            .map_err(|e| format!("GET /api/about (loopback bypass): {e}"))?;
        if !about.status().is_success() {
            let s = about.status();
            let b = about.text().await.unwrap_or_default();
            return Err(format!(
                "loopback /api/about should 200 under --auth=passphrase, got status={s} body={b}"
            ));
        }
        let body: serde_json::Value = about
            .json()
            .await
            .map_err(|e| format!("decode about body: {e}"))?;
        if body.get("auth_mode").and_then(|v| v.as_str()) != Some("passphrase") {
            return Err(format!(
                "expected auth_mode=passphrase on loopback bypass, got {body}"
            ));
        }

        // The structured view REST surface lives under the same wall, so the
        // bypass must extend to it. A successful 200 on `/api/sessions`
        // from loopback without a session is what unblocks the local
        // TUI structured view attach in the issue report.
        let sessions = client
            .get(format!("{base}/api/sessions"))
            .send()
            .await
            .map_err(|e| format!("GET /api/sessions (loopback bypass): {e}"))?;
        if !sessions.status().is_success() {
            let s = sessions.status();
            let b = sessions.text().await.unwrap_or_default();
            return Err(format!(
                "loopback /api/sessions should 200 under --auth=passphrase, got status={s} body={b}"
            ));
        }

        Ok(())
    });

    let _ = h.run_cli(&["serve", "--stop"]);

    if let Err(e) = result {
        panic!("{e}");
    }
}
