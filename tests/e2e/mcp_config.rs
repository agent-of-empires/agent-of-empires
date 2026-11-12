//! `aoe mcp config`: the printed client entry reaches the daemon's `/mcp` server.

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(f)
}

/// POST one JSON-RPC message to `url` with `headers`; returns the status and the parsed body.
async fn rpc(url: &str, headers: &[(String, String)], body: Value) -> (StatusCode, Value) {
    let mut req = Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    for (name, value) in headers {
        req = req.header(name, value);
    }
    let resp = req
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {url}: {e}"));
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "e2e", "version": "1" }
        }
    })
}

#[test]
#[parallel]
fn cli_mcp_config_entry_connects_to_the_daemon() {
    let mut h = TuiTestHarness::new("mcp_config_connects");
    h.append_config("[web]\nmcp_enabled = true");
    h.stop_daemon_on_drop();
    let port = h.start_daemon_with(&[]);

    let out = h.run_cli(&["mcp", "config"]);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("mcp_enabled"),
        "no route warning with the setting on: {stderr}"
    );
    let printed: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let entry = &printed["mcpServers"]["aoe"];
    assert_eq!(entry["type"], "http");
    let url = entry["url"].as_str().expect("url string");
    assert!(url.ends_with(&format!(":{port}/mcp")), "{url}");
    let headers: Vec<(String, String)> = entry["headers"]
        .as_object()
        .expect("token-auth daemon yields headers")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("header string").to_string()))
        .collect();

    block_on(async {
        let (status, _) = rpc(url, &[], initialize()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the header is what authorizes"
        );

        let (status, body) = rpc(url, &headers, initialize()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["result"]["serverInfo"]["name"], "aoe", "{body}");

        let (status, body) = rpc(
            url,
            &headers,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"list_sessions"), "{names:?}");
    });
}

#[test]
#[parallel]
fn cli_mcp_config_guides_when_no_daemon_or_setting_is_off() {
    let mut h = TuiTestHarness::new("mcp_config_guidance");
    let stderr = h.run_cli_err(&["mcp", "config"]);
    assert!(stderr.contains("aoe serve --daemon"), "{stderr}");

    h.stop_daemon_on_drop();
    let port = h.start_daemon();
    let out = h.run_cli(&["mcp", "config"]);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("web.mcp_enabled is off"), "{stderr}");
    let printed: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert!(
        printed["mcpServers"]["aoe"].get("headers").is_none(),
        "a --no-auth daemon has no token to send: {printed}"
    );

    // The warning is accurate: with the setting off there is no `/mcp` route, and a `web`
    // build's GET-only dashboard fallback answers the POST with 405 instead of 404.
    let url = format!("http://127.0.0.1:{port}/mcp");
    let (status, _) = block_on(rpc(&url, &[], initialize()));
    assert!(
        matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ),
        "{status}"
    );
}

/// Rewrite `[web] mcp_enabled` in the harness config without touching the running daemon.
fn set_mcp_enabled(h: &TuiTestHarness, enabled: bool) {
    let path = app_dir_in(h.home_path()).join("config.toml");
    let raw = std::fs::read_to_string(&path).expect("read config.toml");
    let from = format!("mcp_enabled = {}", !enabled);
    assert!(raw.contains(&from), "config has {from}: {raw}");
    let to = format!("mcp_enabled = {enabled}");
    std::fs::write(&path, raw.replace(&from, &to)).expect("write config.toml");
}

fn headers_of(entry: &Value) -> Vec<(String, String)> {
    entry["headers"]
        .as_object()
        .map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_str().expect("header string").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// The daemon reads `web.mcp_enabled` only at startup, so the guidance follows the live route.
#[test]
#[parallel]
fn cli_mcp_config_reports_the_live_route_after_the_setting_changes() {
    // Turned on after startup: the route is still absent and a restart applies it.
    let mut h = TuiTestHarness::new("mcp_config_enabled_since_start");
    h.append_config("[web]\nmcp_enabled = false");
    h.stop_daemon_on_drop();
    h.start_daemon();
    set_mcp_enabled(&h, true);
    let out = h.run_cli(&["mcp", "config"]);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("does not serve /mcp"), "{stderr}");
    assert!(stderr.contains("started before"), "{stderr}");
    assert!(stderr.contains("aoe serve --restart"), "{stderr}");

    // Turned off after startup: the route still answers, and only the next restart drops it.
    let mut h = TuiTestHarness::new("mcp_config_disabled_since_start");
    h.append_config("[web]\nmcp_enabled = true");
    h.stop_daemon_on_drop();
    h.start_daemon_with(&[]);
    set_mcp_enabled(&h, false);
    let out = h.run_cli(&["mcp", "config"]);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("next daemon restart"), "{stderr}");
    assert!(!stderr.contains("does not serve"), "{stderr}");
    let printed: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let entry = &printed["mcpServers"]["aoe"];
    let url = entry["url"].as_str().expect("url string");
    let (status, body) = block_on(rpc(url, &headers_of(entry), initialize()));
    assert_eq!(
        status,
        StatusCode::OK,
        "the live route still serves: {body}"
    );
}

/// The passphrase wall covers loopback, so a headerless entry would get
/// `401 login_required`. The command refuses with guidance and the wall stays in place.
#[test]
#[parallel]
fn cli_mcp_config_refuses_a_passphrase_daemon_on_loopback() {
    let mut h = TuiTestHarness::new("mcp_config_passphrase_loopback");
    h.append_config("[web]\nmcp_enabled = true");
    h.stop_daemon_on_drop();
    let port = h.start_daemon_with(&["--auth", "passphrase", "--passphrase", "e2e-pass"]);
    let out = h.run_cli(&["mcp", "config"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "no entry is printed: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("loopback"), "{stderr}");
    assert!(stderr.contains("--auth token"), "{stderr}");

    let (status, body) = block_on(rpc(
        &format!("http://127.0.0.1:{port}/mcp"),
        &[],
        initialize(),
    ));
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "login_required", "{body}");
}

/// Behind a proxy the same wall still covers loopback. The command refuses and a
/// credential-less request is `401 login_required`.
#[test]
#[parallel]
fn cli_mcp_config_refuses_a_passphrase_daemon_behind_a_proxy() {
    let mut h = TuiTestHarness::new("mcp_config_passphrase_proxy");
    h.append_config("[web]\nmcp_enabled = true");
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
    let out = h.run_cli(&["mcp", "config"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "no entry is printed: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("loopback"), "{stderr}");
    assert!(stderr.contains("--auth token"), "{stderr}");

    // The premise holds: a credential-less /mcp request from this host is walled off.
    // `rpc` reads the body inside the same runtime as the send.
    let (status, body) = block_on(rpc(
        &format!("http://127.0.0.1:{port}/mcp"),
        &[("host".into(), "aoe.example.test".into())],
        initialize(),
    ));
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "login_required", "{body}");
}

/// One daemon spans profiles, so profile-local overrides must not change global
/// listener opt-ins, including after a fresh acquisition replaces a lost owner.
#[test]
#[parallel]
fn configured_services_survive_tui_autostart() {
    crate::harness::require_tmux!();
    if !crate::serve::isolated_local_network() {
        return;
    }
    for enabled in [true, false] {
        let mut h = TuiTestHarness::new_in_tmp("configured_autostart");
        h.append_config(&format!("[web]\nmcp_enabled = {enabled}"));
        h.stop_daemon_on_drop();
        h.run_cli_ok(&["migrate"]);
        let app = app_dir_in(h.home_path());
        std::fs::write(
            app.join("profiles/default/config.toml"),
            format!("[web]\nmcp_enabled = {}", !enabled),
        )
        .unwrap();
        h.spawn_tui();
        h.wait_for_ready();

        let launch: Value = serde_json::from_slice(
            &std::fs::read(app_dir_in(h.home_path()).join("serve.launch")).unwrap(),
        )
        .unwrap();
        assert_eq!(launch["core_only"], !enabled, "configured service ignored");
        assert_eq!(launch["host"], "127.0.0.1");
        assert_eq!(launch["remote"], false);
        if enabled {
            assert_mcp_connects(&h);
        } else {
            assert!(!app_dir_in(h.home_path()).join("serve.url").exists());
            let error = h.run_cli_err(&["mcp", "config"]);
            assert!(error.contains("core-only"), "{error}");
        }
        h.kill_tui();
        h.run_cli_ok(&["serve", "--stop"]);
        h.spawn_tui();
        h.wait_for_ready();
        if enabled {
            assert_mcp_connects(&h);
        } else {
            assert!(!app.join("serve.url").exists());
        }
        h.kill_tui();
        h.run_cli_ok(&["serve", "--stop"]);

        h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
        h.run_cli_ok(&["serve", "--restart"]);
        assert!(h.run_cli_err(&["mcp", "config"]).contains("core-only"));
        h.run_cli_ok(&["serve", "--daemon"]);
        if enabled {
            assert_mcp_connects(&h);
        } else {
            let out = h.run_cli(&["mcp", "config"]);
            assert!(out.status.success(), "{out:?}");
            assert!(String::from_utf8_lossy(&out.stderr).contains("web.mcp_enabled is off"));
            let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
            let entry = &printed["mcpServers"]["aoe"];
            let (status, _) = block_on(rpc(
                entry["url"].as_str().unwrap(),
                &headers_of(entry),
                initialize(),
            ));
            assert!(matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ));
        }
        h.run_cli_ok(&["serve", "--rollback"]);
        assert!(h.run_cli_err(&["mcp", "config"]).contains("core-only"));
    }
}

/// A printed URL is not readiness: require tokenless rejection, initialization,
/// and session-tool discovery using the CLI-provided endpoint and credentials.
fn assert_mcp_connects(h: &TuiTestHarness) {
    let out = h.run_cli(&["mcp", "config"]);
    assert!(out.status.success(), "{out:?}");
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    let entry = &printed["mcpServers"]["aoe"];
    let url = entry["url"].as_str().unwrap();
    let headers = headers_of(entry);
    block_on(async {
        let (status, _) = rpc(url, &[], initialize()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = rpc(url, &headers, initialize()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["result"]["serverInfo"]["name"], "aoe");
        let (status, body) = rpc(
            url,
            &headers,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let (status, body) = rpc(
            url,
            &headers,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "list_sessions"));
    });
}

/// A missing configured default is not permission to create a profile for readers.
/// Administrative replay has a separate native-profile admission path.
#[test]
#[parallel]
fn configured_read_acquisition_preserves_profiles_and_one_owner() {
    if !crate::serve::isolated_local_network() {
        return;
    }
    for enabled in [true, false] {
        let mut h = TuiTestHarness::new_in_tmp("configured_read_acquisition");
        h.stop_daemon_on_drop();
        h.append_config(&format!("[web]\nmcp_enabled = {enabled}"));
        let app = app_dir_in(h.home_path());
        let path = app.join("config.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("default_profile = \"missing\"\n{config}")).unwrap();
        std::fs::remove_dir_all(app.join("profiles/default")).unwrap();
        let children: Vec<_> = (0..4).map(|_| h.spawn_cli(&["profile", "list"])).collect();
        let mut announcements = 0;
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "{out:?}");
            assert!(String::from_utf8_lossy(&out.stdout).starts_with("No profiles found."));
            announcements += String::from_utf8_lossy(&out.stderr)
                .lines()
                .filter(|line| line.starts_with("Started aoe daemon (pid "))
                .count();
        }
        assert_eq!(announcements, 1);
        assert_eq!(std::fs::read_dir(app.join("profiles")).unwrap().count(), 0);
        assert!(h
            .run_cli_err(&["list", "--json"])
            .contains("Profile 'missing' does not exist"));
        if enabled {
            assert_mcp_connects(&h);
        } else {
            assert!(h.run_cli_err(&["mcp", "config"]).contains("core-only"));
        }
    }
}

/// Keep the port occupied through both failures; private recovery requires an
/// explicit request before repaired TCP acquisition may expose MCP again.
#[test]
#[parallel]
fn configured_autostart_reports_tcp_bind_failure_without_private_fallback() {
    crate::harness::require_tmux!();
    if !crate::serve::isolated_local_network() {
        return;
    }
    let port = if cfg!(debug_assertions) { 8081 } else { 8080 };
    let occupied = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let mut h = TuiTestHarness::new_in_tmp("configured_tcp_bind_failure");
    h.append_config("[web]\nmcp_enabled = true");
    h.stop_daemon_on_drop();
    h.run_cli_err(&["serve", "--daemon"]);
    h.run_cli_err(&["profile", "list"]);
    h.run_cli_err(&["--no-autostart", "profile", "list"]);
    h.spawn_tui();
    h.wait_for("Not responding");
    h.run_cli_err(&["--no-autostart", "profile", "list"]);

    h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
    h.run_cli_ok(&["--no-autostart", "profile", "list"]);
    h.send_keys("r");
    h.wait_for_runtime_ready();
    h.run_cli_err(&["mcp", "config"]);
    h.kill_tui();
    h.run_cli_ok(&["serve", "--stop"]);
    drop(occupied);

    h.run_cli_ok(&["profile", "list"]);
    assert_mcp_connects(&h);
    h.run_cli_ok(&["serve", "--stop"]);
    h.spawn_tui();
    h.wait_for_runtime_ready();
    assert_mcp_connects(&h);
}

/// Invalid configuration cannot establish an automatic service-selection policy.
#[test]
#[parallel]
fn configured_invalid_settings_refuse_fresh_acquisition() {
    if !crate::serve::isolated_local_network() {
        return;
    }
    let mut h = TuiTestHarness::new_in_tmp("configured_invalid_settings");
    h.stop_daemon_on_drop();
    h.append_config("[web]\nmcp_enabled = true");
    h.run_cli_ok(&["migrate"]);
    let app = app_dir_in(h.home_path());
    let config = app.join("config.toml");
    let broken = "[web]\nmcp_enabled = true\nbroken = [\n";
    std::fs::write(&config, broken).unwrap();
    let error = h.run_cli_err(&["profile", "list"]);
    assert!(error.contains(config.to_str().unwrap()), "{error}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), broken);
    assert!(!app.join("serve.pid").exists());
    assert!(!app.join("serve.launch").exists());
}

#[test]
#[parallel]
fn configured_autostart_respects_selected_endpoint_and_opt_out() {
    crate::harness::require_tmux!();
    if !crate::serve::isolated_local_network() {
        return;
    }
    let unused = tokio::net::TcpSocket::new_v4().unwrap();
    unused.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}", unused.local_addr().unwrap());
    for remote in [false, true] {
        for environment in [false, true] {
            let mut h = TuiTestHarness::new_in_tmp("configured_no_spawn");
            h.append_config("[web]\nmcp_enabled = true");
            h.stop_daemon_on_drop();
            let args = if environment {
                h.set_env(
                    if remote {
                        "AOE_DAEMON_URL"
                    } else {
                        "AOE_NO_AUTOSTART"
                    },
                    if remote { url.as_str() } else { "1" },
                );
                Vec::new()
            } else if remote {
                vec!["--daemon-url", url.as_str()]
            } else {
                vec!["--no-autostart"]
            };
            let mut read_args = vec!["list", "--json"];
            read_args.extend_from_slice(&args);
            h.run_cli_err(&read_args);
            h.spawn(&args);
            h.wait_for(if remote {
                "Could not reach daemon"
            } else {
                "Not responding"
            });
            let app = app_dir_in(h.home_path());
            for name in ["serve.pid", "serve.launch", "daemon/api.sock"] {
                assert!(!app.join(name).exists(), "unexpected {name}");
            }
        }
    }
}

#[test]
#[parallel]
fn configured_autostart_keeps_security_policy() {
    crate::harness::require_tmux!();
    if !crate::serve::isolated_local_network() {
        return;
    }
    for cityhall in [false, true] {
        let mut h = TuiTestHarness::new_in_tmp("configured_security");
        h.append_config("[web]\nmcp_enabled = true");
        h.stop_daemon_on_drop();
        if cityhall {
            h.set_env("AOE_CITYHALL_MODE", "1");
        } else {
            h.set_env("AOE_SERVE_PASSPHRASE", "configured-local-passphrase");
        }
        h.spawn_tui();
        h.wait_for_ready();
        let raw = std::fs::read_to_string(app_dir_in(h.home_path()).join("serve.url")).unwrap();
        let mut url = reqwest::Url::parse(raw.lines().next().unwrap()).unwrap();
        let token = url
            .query_pairs()
            .find(|(key, _)| key == "token")
            .unwrap()
            .1
            .into_owned();
        url.set_path("/mcp");
        url.set_query(None);
        let (status, body) = block_on(rpc(
            url.as_str(),
            &[("authorization".into(), format!("Bearer {token}"))],
            initialize(),
        ));
        if cityhall {
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(body["error"], "cityhall_mode");
        } else {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(body["error"], "login_required");
            assert!(h.run_cli_err(&["mcp", "config"]).contains("passphrase"));
            crate::serve::assert_original_passphrase_login(&h, "configured-local-passphrase");
            h.kill_tui();
            h.remove_env("AOE_SERVE_PASSPHRASE");
            h.run_cli_ok(&["serve", "--restart"]);
            crate::serve::assert_original_passphrase_login(&h, "configured-local-passphrase");
        }
    }
}

#[cfg(feature = "web")]
#[test]
#[parallel]
fn configured_mcp_respects_disabled_web_across_acquisition_and_reconnect() {
    crate::harness::require_tmux!();
    if !crate::serve::isolated_local_network() {
        return;
    }
    let mut h = TuiTestHarness::new_in_tmp("configured_disabled_web");
    h.append_config("[web]\nmcp_enabled = true");
    h.stop_daemon_on_drop();
    let app = app_dir_in(h.home_path());
    let port = if cfg!(debug_assertions) { 8081 } else { 8080 };
    let assert_private = |h: &TuiTestHarness| {
        assert!(h
            .run_cli_ok(&["--no-autostart", "profile", "list"])
            .contains("default"));
        assert!(h.run_cli_err(&["mcp", "config"]).contains("core-only"));
        assert!(!app.join("serve.url").exists());
        let probe = std::net::TcpListener::bind(("0.0.0.0", port))
            .expect("private acquisition must not open TCP");
        drop(probe);
    };

    h.spawn_tui();
    h.wait_for_runtime_ready();
    assert_mcp_connects(&h);
    h.run_cli_ok(&["plugin", "disable", "aoe.web"]);
    h.run_cli_ok(&["serve", "--stop"]);
    h.wait_for("Not responding");
    h.send_keys("r");
    h.wait_for_runtime_ready();
    assert_private(&h);
    h.kill_tui();
    h.run_cli_ok(&["serve", "--stop"]);

    h.run_cli_ok(&["profile", "list"]);
    assert_private(&h);
    h.run_cli_ok(&["serve", "--stop"]);
    h.spawn_tui();
    h.wait_for_runtime_ready();
    assert_private(&h);
    h.kill_tui();
    h.run_cli_ok(&["serve", "--stop"]);

    let refused = h.run_cli_err(&["serve", "--daemon"]);
    assert!(
        refused.contains("unrecognized subcommand 'serve'"),
        "{refused}"
    );
    assert!(!app.join("serve.pid").exists());
    assert!(std::os::unix::net::UnixStream::connect(app.join("daemon/api.sock")).is_err());
    h.run_cli_ok(&["plugin", "enable", "aoe.web"]);
    assert_private(&h);
    h.run_cli_ok(&["serve", "--restart"]);
    assert_private(&h);
    h.run_cli_ok(&["serve", "--stop"]);

    h.run_cli_ok(&["profile", "list"]);
    assert_mcp_connects(&h);
    assert!(std::net::TcpStream::connect(("192.0.2.1", port)).is_err());
}
