//! Settings reads and saves agree on the layer (#4144): a plain read is the
//! served profile over machine-wide, and a plain save lands where it reads.

use agent_of_empires::server::test_support::{build_router_for_test, build_test_app_state};
use agent_of_empires::session::{self, Config};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap()));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn machine_default_tool() -> Option<String> {
    Config::load().unwrap().session.default_tool
}

fn profile_default_tool(profile: &str) -> Option<Value> {
    let overrides = serde_json::to_value(session::load_profile_config(profile).unwrap()).unwrap();
    overrides.pointer("/session/default_tool").cloned()
}

#[tokio::test]
#[serial_test::serial]
async fn settings_read_and_save_agree_on_the_layer() {
    let _home = crate::common::setup_temp_home();
    let app = build_router_for_test(build_test_app_state(Vec::new()));
    let (_, about) = call(&app, "GET", "/api/about", None).await;
    let served = about["profile"]
        .as_str()
        .expect("about names the served profile")
        .to_string();
    assert!(!served.is_empty());
    session::update_config(|config| config.session.default_tool = Some("claude".into())).unwrap();

    // A plain save of a profile-overridable field lands in the served profile.
    let (status, saved) = call(
        &app,
        "PATCH",
        "/api/settings",
        Some(json!({"session": {"default_tool": "codex", "sidebar_position": "right"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["session"]["default_tool"], "codex");
    assert_eq!(profile_default_tool(&served), Some(json!("codex")));
    assert_eq!(machine_default_tool().as_deref(), Some("claude"));
    // A global-only field in the same save goes machine-wide.
    assert_eq!(
        serde_json::to_value(Config::load().unwrap().session.sidebar_position).unwrap(),
        "right"
    );

    // (uri, expected default_tool)
    let reads = [
        ("/api/settings".to_string(), "codex"),
        ("/api/settings?layer=machine".to_string(), "claude"),
        (format!("/api/settings?profile={served}"), "codex"),
    ];
    for (uri, expected) in reads {
        let (status, body) = call(&app, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["session"]["default_tool"], expected, "{uri}");
    }

    // An explicit machine-wide save sets the inherited value on purpose.
    let (status, _) = call(
        &app,
        "PATCH",
        "/api/settings?layer=machine",
        Some(json!({"session": {"default_tool": "gemini"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(machine_default_tool().as_deref(), Some("gemini"));
    assert_eq!(profile_default_tool(&served), Some(json!("codex")));

    for uri in [
        "/api/settings?layer=nope",
        "/api/settings?layer=machine&profile=x",
        "/api/settings?profile=..%2F..%2Fescape",
    ] {
        let (status, _) = call(&app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}
