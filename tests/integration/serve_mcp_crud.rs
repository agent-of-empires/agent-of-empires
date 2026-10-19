//! `POST|PUT|DELETE /api/mcp/servers`: AoE-owned global servers are writable, every other layer
//! is refused without touching `mcp.json`.

use agent_of_empires::server::test_support::{build_router_for_test, build_test_app_state};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:5558".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
#[serial_test::serial]
async fn global_servers_are_writable_and_other_layers_are_not() {
    let home = crate::common::setup_temp_home();
    std::fs::write(
        home.path().join(".claude.json"),
        r#"{ "mcpServers": { "native": { "command": "n" } } }"#,
    )
    .unwrap();
    let app = build_router_for_test(build_test_app_state(Vec::new()));
    let mcp_json = || {
        let app_dir = agent_of_empires::session::get_app_dir().unwrap();
        std::fs::read_to_string(app_dir.join("mcp.json")).unwrap_or_default()
    };

    // Create shadows the native server of the same name, and no response leaks the secret.
    let create = json!({
        "name": "native", "transport": "stdio", "command": "mine",
        "env": [{ "name": "TOKEN", "value": "SUPER_SECRET" }]
    });
    let (status, body) = call(&app, Method::POST, "/api/mcp/servers", create.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["provenance"], "global");
    assert!(!body.to_string().contains("SUPER_SECRET"), "{body}");
    let (status, body) = call(&app, Method::POST, "/api/mcp/servers", create).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("mcp_server_exists"))
    );

    let (status, body) = call(&app, Method::GET, "/api/mcp/servers", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["effective"][0]["command"], "mine");
    assert_eq!(body["effective"][0]["provenance"], "global");

    // A valueless secret keeps its stored value.
    let edit = json!({
        "transport": "stdio", "command": "changed", "args": ["-x"], "env": [{ "name": "TOKEN" }]
    });
    let (status, body) = call(&app, Method::PUT, "/api/mcp/servers/native", edit).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["command"], "changed");
    assert!(mcp_json().contains("SUPER_SECRET"));

    let bad = json!({ "transport": "http", "url": "https://e", "headers": [{ "name": "TOKEN" }] });
    let (status, body) = call(&app, Method::PUT, "/api/mcp/servers/native", bad).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_mcp_server"))
    );

    let (status, _) = call(&app, Method::DELETE, "/api/mcp/servers/native", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let before = mcp_json();

    // With the global entry gone, `native` is only the agent-native server: refuse, write nothing.
    let edit = json!({ "transport": "stdio", "command": "x" });
    let (status, body) = call(&app, Method::PUT, "/api/mcp/servers/native", edit).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("mcp_read_only"))
    );
    let (status, body) = call(&app, Method::DELETE, "/api/mcp/servers/native", Value::Null).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("mcp_read_only"))
    );
    let (status, body) = call(&app, Method::DELETE, "/api/mcp/servers/ghost", Value::Null).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("mcp_server_not_found"))
    );
    assert_eq!(mcp_json(), before);
}
