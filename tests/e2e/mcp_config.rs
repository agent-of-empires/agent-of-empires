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
    let mut req = Client::new()
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
