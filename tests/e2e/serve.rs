//! `aoe serve`: the TUI serve dialog, the daemon lifecycle, and the
//! passphrase wall against a real daemon.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use serial_test::parallel;

use crate::harness::{
    app_dir_in, pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness,
};

fn app_file(h: &TuiTestHarness, name: &str) -> PathBuf {
    app_dir_in(h.home_path()).join(name)
}

fn pid_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn read_pid(path: &PathBuf) -> i32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .trim()
        .parse()
        .expect("serve.pid holds an integer")
}

fn wait_for_exit(pid: i32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid), "{what}: PID {pid} still alive");
}

/// 32 bytes of device binding; only its length and encoding matter.
fn device_binding() -> String {
    URL_SAFE_NO_PAD.encode([0x5Au8; 32])
}

/// `aoe_session=...` from the login response, which may carry several cookies.
fn session_cookie(login: &reqwest::Response) -> String {
    login
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let first = v.to_str().ok()?.split(';').next()?.trim().to_string();
            first.starts_with("aoe_session=").then_some(first)
        })
        .expect("login response missing aoe_session Set-Cookie")
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(f)
}

async fn status_of(request: reqwest::RequestBuilder, what: &str) -> (StatusCode, Value) {
    let resp = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{what}: {e}"));
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `R` opens the serve ModePicker with both cards and the deferred-transport
/// hint the Confirm screen relies on; Escape returns to the home screen.
#[test]
#[parallel]
fn tui_serve_dialog_mode_picker_opens_and_escapes() {
    require_tmux!();
    let mut h = TuiTestHarness::new("serve_mode_picker");
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("R");

    h.wait_for("How should this be reachable?");
    h.assert_screen_contains("Local network");
    h.assert_screen_contains("Internet (HTTPS)");
    h.assert_screen_contains("Pick transport on next screen.");

    h.send_keys("Escape");
    h.wait_for("No sessions yet");
}

/// The daemon child binds the port and survives (it must not self-detect its
/// own pre-written PID), logs through the tracing sink rather than a revived
/// `serve.log` (#1124), replays its launch state on `--restart` (#1794), and
/// cleans up its PID file on `--stop`.
#[test]
#[parallel]
fn cli_serve_daemon_lifecycle() {
    let mut h = TuiTestHarness::new("serve_daemon_lifecycle");
    // Stop the daemon even if an assertion panics, so it cannot hold the port.
    h.stop_daemon_on_drop();
    let port = h.start_daemon();
    let pid_path = app_file(&h, "serve.pid");
    let launch_path = app_file(&h, "serve.launch");

    let pid1 = read_pid(&pid_path);
    assert!(
        pid_alive(pid1),
        "child PID {pid1} not alive after port bind"
    );
    assert!(launch_path.exists(), "serve.launch written on start");

    let debug_log = app_file(&h, "debug.log");
    let contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {e}", debug_log.display()));
    assert!(
        contents.contains("[AOE_START_MARKER]"),
        "debug.log should carry the filter-immune startup marker; got: {contents:?}"
    );
    assert!(
        !app_file(&h, "serve.log").exists(),
        "serve.log must not be re-created post-consolidation"
    );

    let restart = h.run_cli(&["serve", "--restart"]);
    assert!(
        restart.status.success(),
        "aoe serve --restart failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&restart.stdout),
        String::from_utf8_lossy(&restart.stderr),
    );
    assert!(
        crate::harness::wait_for_port(port, Duration::from_secs(10)),
        "restarted daemon never rebound the persisted port {port}"
    );
    let pid2 = read_pid(&pid_path);
    assert_ne!(pid1, pid2, "restart should spawn a new daemon PID");
    assert!(
        pid_alive(pid2),
        "restarted daemon PID {pid2} should be alive"
    );
    wait_for_exit(pid1, "old daemon after restart");
    assert!(
        launch_path.exists(),
        "serve.launch rewritten by the restart"
    );

    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(
        stop.status.success(),
        "aoe serve --stop failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr),
    );
    wait_for_exit(pid2, "daemon after --stop");
    assert!(!pid_path.exists(), "serve.pid cleaned up after --stop");
}

/// Under `--auth=passphrase` a remote caller (loopback socket plus a trusted
/// `X-Forwarded-For`) is walled off until it logs in, while a real loopback
/// caller is fs-trusted and gets through with no credential (#1525, #3843).
#[test]
#[parallel]
fn cli_serve_auth_passphrase_wall_and_loopback_bypass() {
    let mut h = TuiTestHarness::new("serve_auth_passphrase");
    // Stop the daemon even if an assertion panics, so it cannot hold the port.
    h.stop_daemon_on_drop();
    let port = h.start_daemon_with(&["--auth", "passphrase", "--passphrase", "e2e-pass"]);
    let binding = device_binding();

    block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        let client = Client::new();

        for path in ["/api/about", "/api/sessions"] {
            let (status, body) = status_of(
                client
                    .get(format!("{base}{path}"))
                    .header("x-forwarded-for", "10.0.0.5"),
                path,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {body}");
            if path == "/api/about" {
                assert_eq!(body["error"].as_str(), Some("login_required"), "{body}");
            }
        }

        let login = client
            .post(format!("{base}/api/login"))
            .json(&json!({ "passphrase": "e2e-pass", "device_binding_secret": binding }))
            .send()
            .await
            .expect("POST /api/login");
        assert!(
            login.status().is_success(),
            "login failed: {}",
            login.status()
        );
        let cookie = session_cookie(&login);

        let (status, body) = status_of(
            client
                .get(format!("{base}/api/about"))
                .header("cookie", &cookie)
                .header("x-aoe-device-binding", &binding),
            "GET /api/about (auth)",
        )
        .await;
        assert!(status.is_success(), "authenticated /api/about: {body}");
        assert_eq!(body["auth_mode"].as_str(), Some("passphrase"), "{body}");

        // Loopback with no cookie, binding, or XFF: the local TUI structured
        // view attach depends on this carve-out covering the REST surface.
        for path in ["/api/about", "/api/sessions"] {
            let (status, body) = status_of(client.get(format!("{base}{path}")), path).await;
            assert!(status.is_success(), "loopback {path} should 200: {body}");
        }
    });
}

/// `--behind-proxy` drops the loopback carve-out: a same-host proxy that sends
/// no forwarding header must not turn the API into a public surface, and
/// signing in over that shape must not count as elevation (#3843).
#[test]
#[parallel]
fn cli_serve_auth_passphrase_behind_proxy_gates_unforwarded_requests() {
    let mut h = TuiTestHarness::new("serve_auth_passphrase_behind_proxy");
    // Stop the daemon even if an assertion panics, so it cannot hold the port.
    h.stop_daemon_on_drop();
    let port = h.start_daemon_with(&[
        "--auth",
        "passphrase",
        "--passphrase",
        "e2e-pass",
        "--behind-proxy",
        "--allowed-host",
        "aoe.example.test",
    ]);
    let binding = device_binding();

    block_on(async {
        let base = format!("http://127.0.0.1:{port}");
        let client = Client::new();
        let proxied = |req: reqwest::RequestBuilder| req.header("host", "aoe.example.test");

        for path in ["/api/sessions", "/api/projects"] {
            let (status, body) =
                status_of(proxied(client.get(format!("{base}{path}"))), path).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{path} must 401 for an unforwarded proxied request: {body}"
            );
        }

        // The login surfaces stay reachable or nobody could get past the wall.
        let (status, _) = status_of(
            proxied(client.get(format!("{base}/api/login/status"))),
            "/api/login/status",
        )
        .await;
        assert!(status.is_success(), "/api/login/status must stay reachable");

        let login = proxied(
            client
                .post(format!("{base}/api/login"))
                .json(&json!({ "passphrase": "e2e-pass", "device_binding_secret": binding })),
        )
        .send()
        .await
        .expect("POST /api/login");
        assert!(
            login.status().is_success(),
            "login failed: {}",
            login.status()
        );
        let cookie = session_cookie(&login);

        // Skill and plugin mutation run host code, so they demand a step-up
        // even for a signed-in proxied caller. The guard runs before the
        // handler, so the named skill is never touched.
        let (status, body) = status_of(
            proxied(client.delete(format!("{base}/api/skills/e2e-nonexistent")))
                .header("cookie", &cookie)
                .header("x-aoe-device-binding", &binding),
            "DELETE /api/skills",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"].as_str(), Some("elevation_required"), "{body}");
    });
}

/// A fatal startup validation failure reaches the tracing sink `aoe logs`
/// reads, not just raw stderr, and still exits non-zero (#2896).
#[test]
#[parallel]
fn cli_serve_startup_bail_reaches_debug_log() {
    let h = TuiTestHarness::new("serve_startup_bail_logged");
    let out = h.run_cli(&["serve", "--behind-proxy"]);
    assert!(
        !out.status.success(),
        "serve --behind-proxy without --allowed-host must exit non-zero.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let debug_log = app_file(&h, "debug.log");
    let contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {e}", debug_log.display()));
    assert!(
        contents.contains("--behind-proxy requires --allowed-host"),
        "fatal startup reason must be routed through the tracing sink; debug.log was:\n{contents}"
    );
}

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| {
            let id = l.trim().strip_prefix("ID:")?.trim();
            (!id.is_empty()).then(|| id.to_string())
        })
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

#[test]
fn parse_session_id_extracts_the_trimmed_value() {
    assert_eq!(
        parse_session_id("  Title:    demo\n  ID:      abc-123\n"),
        "abc-123"
    );
}

#[test]
#[should_panic(expected = "could not find session ID")]
fn parse_session_id_panics_when_id_line_is_missing() {
    parse_session_id("  Title:    demo\n");
}

#[test]
#[should_panic(expected = "could not find session ID")]
fn parse_session_id_panics_when_id_value_is_empty() {
    // A blank `ID:` line (broken `aoe add` output) must fail fast rather
    // than hand `prompt_until_accepted` an empty id to retry for 30s.
    parse_session_id("  Title:    demo\n  ID:      \n");
}

/// `aoe acp prompt` 404s until the worker is live and handshaked, so a
/// successful call is the readiness oracle.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "hello"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "worker never accepted a prompt within {timeout:?}.\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `aoe acp <verb>` (built on `HttpClient`, see `src/acp/client/http.rs`)
/// authenticates against a `--behind-proxy --auth=passphrase` daemon by
/// logging in via `POST /api/login` with the daemon's own `serve.passphrase`
/// file (the same file `aoe serve --restart` reads) and caching the
/// resulting `aoe_session` cookie, mirroring how it reuses `serve.token` for
/// `--auth=token`. This matters specifically under `--behind-proxy`: the
/// loopback auth bypass (`cli_serve_auth_passphrase_loopback_bypass`) is
/// withdrawn there
/// (`cli_serve_auth_passphrase_behind_proxy_gates_unforwarded_requests`), so
/// a same-host, same-user caller has no other way in.
#[test]
#[parallel]
fn cli_acp_prompt_authenticates_against_behind_proxy_passphrase_daemon() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("acp_prompt_passphrase_behind_proxy");
    let script_path = h.home_path().join("passphrase-prompt-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

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
        assert!(out.status.success(), "git {args:?} failed");
    }

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
        "--behind-proxy",
        "--allowed-host",
        "aoe.example.test",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase --behind-proxy failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "passphrase-prompt",
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
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));
}

/// The WS half of the passphrase-login fallback: `aoe acp tail` (and the
/// TUI's live structured-view stream, same `src/acp/client/ws.rs`) must
/// offer the literal `aoe-auth` subprotocol alongside
/// `aoe-device.<secret>`. The daemon (`src/server/acp_ws.rs`) only echoes a
/// `Sec-WebSocket-Protocol` response header when the client offered that
/// literal, and tungstenite hard-fails the handshake
/// (`SubProtocolError::NoSubProtocol`) when the response omits it while the
/// request carried one (RFC 6455).
#[test]
#[parallel]
fn cli_acp_tail_connects_to_behind_proxy_passphrase_daemon() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("acp_tail_passphrase_behind_proxy");
    let script_path = h.home_path().join("passphrase-tail-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

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
        assert!(out.status.success(), "git {args:?} failed");
    }

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
        "--behind-proxy",
        "--allowed-host",
        "aoe.example.test",
    ]);
    assert!(
        start.status.success(),
        "aoe serve --daemon --auth=passphrase --behind-proxy failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "passphrase-tail",
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
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // `tail` replays history from seq 0 by default, so a completed handshake
    // is observable as a printed frame line; a failed one closes stdout
    // (EOF) without ever printing one.
    let mut child = h.spawn_cli(&["acp", "tail", &session_id]);
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let _ = tx.send(std::io::BufReader::new(stdout).lines().next());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Some(Ok(line))) => {
            assert!(
                !line.trim().is_empty(),
                "`aoe acp tail` printed an empty line instead of a frame"
            );
            let _ = child.kill();
            let _ = child.wait();
        }
        other => {
            use std::io::Read;
            let _ = child.kill();
            let status = child.wait().expect("wait for tail process");
            let mut stderr = String::new();
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_string(&mut stderr);
            }
            panic!(
                "`aoe acp tail` never streamed a frame ({other:?}); exited {status}; stderr:\n{stderr}"
            );
        }
    }
}
