//! `/api` JSON is compressed per `Accept-Encoding` through the real router.

use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state_with_policy,
};
use agent_of_empires::session::Instance;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request};
use axum::Router;
use std::io::Read;
use tower::ServiceExt;

const SESSIONS: usize = 300;

fn sessions() -> Vec<Instance> {
    (0..SESSIONS)
        .map(|i| {
            let mut inst = Instance::new(&format!("session {i}"), "/tmp/aoe-compression-test");
            inst.id = format!("id-{i}");
            inst
        })
        .collect()
}

/// One router per case so every request sees the same state.
fn router(count: usize) -> Router {
    let allowed = vec!["localhost".to_string()];
    let state = build_test_app_state_with_policy(
        sessions().into_iter().take(count).collect(),
        allowed,
        Vec::new(),
        None,
    );
    build_router_for_test(state)
}

/// Status-checked `(content-encoding, vary, body)` of `GET /api/sessions`.
async fn fetch(app: &Router, accept_encoding: Option<&str>) -> (Option<String>, String, Vec<u8>) {
    let mut req = Request::builder()
        .uri("/api/sessions")
        .header("host", "localhost");
    if let Some(value) = accept_encoding {
        req = req.header(header::ACCEPT_ENCODING, value);
    }
    let mut req = req.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:5555".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let header_str = |name| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let encoding = header_str(header::CONTENT_ENCODING);
    let vary = header_str(header::VARY).unwrap_or_default();
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (encoding, vary, body.to_vec())
}

#[tokio::test]
#[serial_test::parallel]
async fn sessions_json_is_compressed_when_negotiated() {
    let app = router(SESSIONS);
    let (encoding, _, plain) = fetch(&app, None).await;
    assert_eq!(encoding, None);
    let plain_json: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(plain_json["sessions"].as_array().unwrap().len(), SESSIONS);

    let (encoding, vary, gz) = fetch(&app, Some("gzip")).await;
    assert_eq!(encoding.as_deref(), Some("gzip"));
    assert!(
        vary.to_ascii_lowercase().contains("accept-encoding"),
        "{vary}"
    );
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut decoded)
        .unwrap();
    assert!(decoded == plain);

    let (encoding, _, zst) = fetch(&app, Some("zstd, gzip")).await;
    assert_eq!(encoding.as_deref(), Some("zstd"));
    assert!(zstd::decode_all(&zst[..]).unwrap() == plain);

    eprintln!(
        "/api/sessions x{SESSIONS}: identity={} gzip={} zstd={}",
        plain.len(),
        gz.len(),
        zst.len()
    );
    assert!(
        gz.len() * 4 < plain.len(),
        "gzip {} vs {}",
        gz.len(),
        plain.len()
    );
    assert!(
        zst.len() * 4 < plain.len(),
        "zstd {} vs {}",
        zst.len(),
        plain.len()
    );
}

#[tokio::test]
#[serial_test::parallel]
async fn tiny_json_is_left_alone() {
    let (encoding, _, body) = fetch(&router(0), Some("gzip, zstd")).await;
    assert_eq!(encoding, None);
    assert!(body.len() < 100, "{} bytes", body.len());
    serde_json::from_slice::<serde_json::Value>(&body).unwrap();
}
