//! HTTP contract of the `/api/suggestions` handlers through the real router:
//! status, error code, query decoding and response shape. Store logic is
//! covered in `plugin::suggestions`.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::plugin::host::PluginHost;
use crate::session::projects::{self, Project, ProjectScope};

const PROVIDER: &str = "acme.gh";

const MANIFEST: &str = r#"
id = "acme.gh"
name = "GitHub"
version = "1.0.0"
api_version = 20
capabilities = ["runtime.worker"]

[runtime]
kind = "command"
system = true
command = ["sleep", "600"]

[suggestions]

[[suggestions.sources]]
criterion = "assigned"
label = "Assigned"
variables = ["number"]
"#;

struct Fixture {
    app: axum::Router,
    host: Arc<PluginHost>,
    repo: PathBuf,
    // Field order is drop order: restore the env, then reload the registry
    // from it, then remove the directory.
    _env: crate::session::test_support::EnvGuard,
    _reload: crate::plugin::ReloadRegistryOnDrop,
    _temp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let env = crate::session::test_support::EnvGuard::set(&[
        ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
        ("HOME", temp.path().to_path_buf()),
        ("USERPROFILE", temp.path().to_path_buf()),
    ]);
    let dir = crate::plugin::plugins_dir().unwrap().join(PROVIDER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("aoe-plugin.toml"), MANIFEST).unwrap();
    crate::session::update_config(|config| {
        config.plugins.insert(
            PROVIDER.to_string(),
            crate::session::PluginConfig {
                grant: Some(crate::session::CapabilityGrant {
                    manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(MANIFEST.as_bytes()),
                    capabilities: vec!["runtime.worker".to_string()],
                    granted_at: chrono::Utc::now(),
                }),
                ..crate::session::PluginConfig::default()
            },
        );
    })
    .unwrap();
    let reload = crate::plugin::ReloadRegistryOnDrop;
    crate::plugin::reload_registry();

    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    projects::add(
        "default",
        ProjectScope::Global,
        Project::new("repo", repo.to_string_lossy(), ProjectScope::Global),
        false,
    )
    .unwrap();

    let host = PluginHost::new(
        &temp.path().join("host"),
        "default",
        None,
        Arc::new(crate::server::runtime::RuntimeWork::default()),
    )
    .unwrap();
    let state = crate::server::test_support::build_test_app_state_with_policy_configured(
        Vec::new(),
        vec!["localhost".into()],
        Vec::new(),
        None,
        {
            let host = host.clone();
            move |state| state.plugin_host = Some(host)
        },
    );
    Fixture {
        app: crate::server::test_support::build_router_for_test(state),
        host,
        repo,
        _env: env,
        _reload: reload,
        _temp: temp,
    }
}

impl Fixture {
    async fn call(&self, method: Method, uri: &str, body: Value) -> (StatusCode, String, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            crate::server::peer::ConnectionPeer::UnixOwner {
                uid: nix::unistd::geteuid().as_raw(),
            },
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let code = body["error"].as_str().unwrap_or_default().to_string();
        (status, code, body)
    }

    /// Publish `id` the way a worker would, with no hint and no mapping key.
    fn seed_item(&self, id: &str, mapping_key: Option<&str>, number: &str) {
        let mut item = json!({
            "id": id,
            "title": "Fix it",
            "reason": "assigned to you",
            "source_url": "https://example.com/1",
            "source_ref": "acme/web#42",
            "preview": "body",
            "criterion": "assigned",
            "variables": { "number": number },
            "draft": { "title": "Fix #42", "prompt": "Fix #{number}" },
        });
        if let Some(key) = mapping_key {
            item["mapping_key"] = json!(key);
        }
        self.host.with_suggestions_db(|conn, profile| {
            conn.execute(
                "INSERT OR REPLACE INTO suggestion_providers (profile, provider_id, updated_at)
                 VALUES (?1, ?2, 0)",
                [profile, PROVIDER],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO suggestion_items (profile, provider_id, item_id, item_json)
                 VALUES (?1, ?2, ?3, ?4)",
                [profile, PROVIDER, id, &item.to_string()],
            )
            .unwrap();
        });
    }
}

#[tokio::test]
#[serial_test::serial]
async fn snapshot_and_refresh() {
    let f = fixture();
    f.seed_item("a", None, "42");

    let (status, _, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["providers"][0]["id"], PROVIDER);
    assert_eq!(body["providers"][0]["sources"][0]["criterion"], "assigned");
    assert_eq!(body["items"][0]["id"], "a");
    assert_eq!(body["items"][0]["dismissed"], false);
    assert_eq!(body["prefs"]["refresh_minutes"], 15);

    for uri in ["/api/suggestions/refresh", "/api/suggestions/checks/run"] {
        let (status, _, body) = f.call(Method::POST, uri, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body, json!({ "ok": true, "notified": 0 }), "{uri}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn dismiss_and_undismiss() {
    let f = fixture();
    f.seed_item("a", None, "42");
    let item = |provider: &str, id: &str| json!({ "provider_id": provider, "id": id });

    let (status, _, body) = f
        .call(
            Method::POST,
            "/api/suggestions/dismiss",
            item(PROVIDER, "a"),
        )
        .await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["items"][0]["dismissed"], true);

    let (status, _, _) = f
        .call(
            Method::POST,
            "/api/suggestions/undismiss",
            item(PROVIDER, "a"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["items"][0]["dismissed"], false);

    for uri in ["/api/suggestions/dismiss", "/api/suggestions/undismiss"] {
        let (status, code, _) = f.call(Method::POST, uri, item(PROVIDER, "gone")).await;
        assert_eq!(
            (status, code.as_str()),
            (StatusCode::CONFLICT, "stale_suggestion"),
            "{uri}"
        );
        let (status, code, _) = f.call(Method::POST, uri, item("nobody", "a")).await;
        assert_eq!(
            (status, code.as_str()),
            (StatusCode::NOT_FOUND, "unknown_provider"),
            "{uri}"
        );
        let (status, _, _) = f.call(Method::POST, uri, json!({ "id": "a" })).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn sources_and_prefs() {
    let f = fixture();
    let source = |provider: &str, criterion: &str| json!({ "provider_id": provider, "criterion": criterion, "enabled": false });

    let (status, _, body) = f
        .call(
            Method::PATCH,
            "/api/suggestions/sources",
            source(PROVIDER, "assigned"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, source(PROVIDER, "assigned"));
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["providers"][0]["sources"][0]["enabled"], false);

    for (body, status, code) in [
        (
            source(PROVIDER, "undeclared"),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            source("nobody", "assigned"),
            StatusCode::NOT_FOUND,
            "unknown_provider",
        ),
    ] {
        let (got, got_code, _) = f
            .call(Method::PATCH, "/api/suggestions/sources", body)
            .await;
        assert_eq!((got, got_code.as_str()), (status, code));
    }

    let (status, _, body) = f
        .call(
            Method::PATCH,
            "/api/suggestions/prefs",
            json!({ "sidebar": true, "refresh_minutes": 30 }),
        )
        .await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["prefs"]["sidebar"], true);
    assert_eq!(snapshot["prefs"]["refresh_minutes"], 30);

    let (status, code, _) = f
        .call(
            Method::PATCH,
            "/api/suggestions/prefs",
            json!({ "refresh_minutes": 7 }),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["prefs"]["refresh_minutes"], 30);
}

#[tokio::test]
#[serial_test::serial]
async fn templates_round_trip_and_validation() {
    let f = fixture();
    let key = json!({ "provider_id": PROVIDER, "criterion": "assigned" });
    let query = format!("/api/suggestions/templates?provider_id={PROVIDER}&criterion=assigned");

    let (status, _, body) = f.call(Method::GET, &query, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&body["default"], &body["template"]),
        (&json!(true), &Value::Null)
    );

    let (status, _, body) = f
        .call(
            Method::PUT,
            "/api/suggestions/templates",
            json!({ "provider_id": PROVIDER, "criterion": "assigned", "template": "Look at {number}" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "provider_id": PROVIDER, "criterion": "assigned",
                "template": "Look at {number}", "default": false })
    );
    let (_, _, body) = f.call(Method::GET, &query, Value::Null).await;
    assert_eq!(body["template"], "Look at {number}");
    let (_, _, body) = f
        .call(Method::GET, "/api/suggestions/templates", Value::Null)
        .await;
    assert_eq!(
        body,
        json!({ "templates": [{ "provider_id": PROVIDER, "criterion": "assigned",
                                "template": "Look at {number}" }] })
    );

    for (uri, status, code) in [
        (
            format!("/api/suggestions/templates?provider_id={PROVIDER}"),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            format!("/api/suggestions/templates?provider_id={PROVIDER}&criterion=undeclared"),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            "/api/suggestions/templates?provider_id=nobody&criterion=assigned".to_string(),
            StatusCode::NOT_FOUND,
            "unknown_provider",
        ),
    ] {
        let (got, got_code, _) = f.call(Method::GET, &uri, Value::Null).await;
        assert_eq!((got, got_code.as_str()), (status, code), "{uri}");
    }
    let (status, code, _) = f
        .call(
            Method::PUT,
            "/api/suggestions/templates",
            json!({ "provider_id": PROVIDER, "criterion": "undeclared", "template": "x" }),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );

    let (status, code, _) = f
        .call(
            Method::PUT,
            "/api/suggestions/templates",
            json!({ "provider_id": PROVIDER, "criterion": "assigned",
                    "template": "x".repeat(65 * 1024) }),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );

    for _ in 0..2 {
        let (status, _, body) = f
            .call(Method::DELETE, "/api/suggestions/templates", key.clone())
            .await;
        assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));
    }
    let (_, _, body) = f.call(Method::GET, &query, Value::Null).await;
    assert_eq!(body["default"], true);
}

#[tokio::test]
#[serial_test::serial]
async fn prepare_resolves_prompt_template_and_stale_items() {
    let f = fixture();
    f.seed_item("a", None, "42");
    let item = |provider: &str, id: &str| json!({ "provider_id": provider, "id": id });

    let (status, _, body) = f
        .call(
            Method::POST,
            "/api/suggestions/prepare",
            item(PROVIDER, "a"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Fix #42");
    // Only a template expands variables; the draft prompt is used as published.
    assert_eq!(body["initial_prompt"], "Fix #{number}");
    assert_eq!(body["source_ref"], "acme/web#42");
    assert_eq!(body["project"], Value::Null);

    f.call(
        Method::PUT,
        "/api/suggestions/templates",
        json!({ "provider_id": PROVIDER, "criterion": "assigned", "template": "Review {number}" }),
    )
    .await;
    let (_, _, body) = f
        .call(
            Method::POST,
            "/api/suggestions/prepare",
            item(PROVIDER, "a"),
        )
        .await;
    assert_eq!(body["initial_prompt"], "Review 42");

    // Each value is under the publish bound, but the expansion exceeds the prompt limit.
    f.seed_item("big", None, &"n".repeat(2000));
    f.call(
        Method::PUT,
        "/api/suggestions/templates",
        json!({ "provider_id": PROVIDER, "criterion": "assigned", "template": "{number}".repeat(100) }),
    )
    .await;
    let (status, code, _) = f
        .call(
            Method::POST,
            "/api/suggestions/prepare",
            item(PROVIDER, "big"),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );

    for (body, status, code) in [
        (
            item(PROVIDER, "gone"),
            StatusCode::CONFLICT,
            "stale_suggestion",
        ),
        (
            item("nobody", "a"),
            StatusCode::NOT_FOUND,
            "unknown_provider",
        ),
    ] {
        let (got, got_code, _) = f.call(Method::POST, "/api/suggestions/prepare", body).await;
        assert_eq!((got, got_code.as_str()), (status, code));
    }
}

#[tokio::test]
#[serial_test::serial]
async fn mappings_validate_list_and_delete_by_query() {
    let f = fixture();
    let repo = f.repo.to_string_lossy().into_owned();
    let put = |provider: &str, key: &str, projects: Value| json!({ "provider_id": provider, "mapping_key": key, "projects": projects });

    for (body, status, code) in [
        (
            put("nobody", "acme/web", json!([repo])),
            StatusCode::NOT_FOUND,
            "unknown_provider",
        ),
        (
            put(PROVIDER, "acme/web", json!([])),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            put(PROVIDER, "acme/web", json!(["/not/registered"])),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            put(PROVIDER, "", json!([repo])),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            put(PROVIDER, "acme/web", json!(vec![repo.clone(); 17])),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            put(PROVIDER, "acme/web", json!([repo, repo])),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
    ] {
        let (got, got_code, _) = f
            .call(Method::PUT, "/api/suggestions/mappings", body.clone())
            .await;
        assert_eq!((got, got_code.as_str()), (status, code), "{body}");
    }
    let (_, _, listed) = f
        .call(Method::GET, "/api/suggestions/mappings", Value::Null)
        .await;
    assert_eq!(listed, json!({ "mappings": [] }));

    let (status, _, stored) = f
        .call(
            Method::PUT,
            "/api/suggestions/mappings",
            put(PROVIDER, "acme/web", json!([repo])),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored["mapping_key"], "acme/web");
    assert_eq!(stored["projects"].as_array().unwrap().len(), 1);

    // A row of a provider that is no longer active stays listed and removable.
    f.host.with_suggestions_db(|conn, profile| {
        crate::plugin::suggestions::put_mapping(
            conn,
            profile,
            "gone.plugin",
            "acme/old",
            &["/old".to_string()],
        )
        .unwrap();
    });
    let (_, _, listed) = f
        .call(Method::GET, "/api/suggestions/mappings", Value::Null)
        .await;
    let rows: Vec<(&str, &str)> = listed["mappings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["provider_id"].as_str().unwrap(),
                m["mapping_key"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(rows, [(PROVIDER, "acme/web"), ("gone.plugin", "acme/old")]);

    // A mapping makes prepare resolve the project.
    f.seed_item("a", Some("acme/web"), "42");
    let (_, _, prepared) = f
        .call(
            Method::POST,
            "/api/suggestions/prepare",
            json!({ "provider_id": PROVIDER, "id": "a" }),
        )
        .await;
    assert_eq!(prepared["project"]["name"], "repo");

    let delete = "/api/suggestions/mappings?provider_id=gone.plugin&mapping_key=acme%2Fold";
    for deleted in [true, false] {
        let (status, _, body) = f.call(Method::DELETE, delete, Value::Null).await;
        assert_eq!(
            (status, body),
            (StatusCode::OK, json!({ "ok": true, "deleted": deleted }))
        );
    }
    let (status, _, _) = f
        .call(
            Method::DELETE,
            "/api/suggestions/mappings?provider_id=x",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, _, listed) = f
        .call(Method::GET, "/api/suggestions/mappings", Value::Null)
        .await;
    assert_eq!(listed["mappings"].as_array().unwrap().len(), 1);
}
