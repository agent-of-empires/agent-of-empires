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
api_version = 23
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
    state: Arc<crate::server::state::AppState>,
    host: Arc<PluginHost>,
    repo: PathBuf,
    // Field order is drop order: restore the env, then reload the registry
    // from it, then remove the directory.
    _env: crate::session::test_support::EnvGuard,
    _reload: crate::plugin::ReloadRegistryOnDrop,
    _temp: tempfile::TempDir,
}

/// Installs and grants the fixture provider in the isolated app dir.
pub(crate) fn register_provider() -> crate::plugin::ReloadRegistryOnDrop {
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
                    granted_at: crate::time::Timestamp::now(),
                }),
                ..crate::session::PluginConfig::default()
            },
        );
    })
    .unwrap();
    let reload = crate::plugin::ReloadRegistryOnDrop;
    crate::plugin::reload_registry();
    reload
}

fn fixture() -> Fixture {
    fixture_with(Vec::new(), false)
}

fn fixture_with(prior: Vec<crate::session::Instance>, cityhall: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let env = crate::session::test_support::EnvGuard::set(&[
        ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
        ("HOME", temp.path().to_path_buf()),
        ("USERPROFILE", temp.path().to_path_buf()),
    ]);
    let reload = register_provider();

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
        prior,
        vec!["localhost".into()],
        Vec::new(),
        None,
        {
            let host = host.clone();
            move |state| {
                state.plugin_host = Some(host);
                state.cityhall_mode = cityhall;
            }
        },
    );
    Fixture {
        app: crate::server::test_support::build_router_for_test(state.clone()),
        state,
        host,
        repo,
        _env: env,
        _reload: reload,
        _temp: temp,
    }
}

impl Fixture {
    fn seed_item(&self, id: &str, mapping_key: Option<&str>, number: &str) {
        seed_item_on(&self.host, id, mapping_key, number);
    }

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
}

/// Records `session_id` as the settled primary Session of item `id`.
pub(crate) fn link_session_on(host: &PluginHost, id: &str, session_id: &str) {
    host.with_suggestions_db(|conn, profile| {
        crate::plugin::suggestions::claim_link(
            conn,
            profile,
            PROVIDER,
            id,
            session_id,
            crate::plugin::suggestions::StartedVia::Wizard,
            &|_| false,
        )
        .unwrap();
        crate::plugin::suggestions::settle_link(conn, profile, PROVIDER, id, session_id).unwrap();
    });
}

/// Publish `id` the way a worker would, with no hint and no mapping key.
pub(crate) fn seed_item_on(host: &PluginHost, id: &str, mapping_key: Option<&str>, number: &str) {
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
    let hash = crate::plugin::suggestions::spec_hash(
        &crate::plugin::registry()
            .get(PROVIDER)
            .and_then(|p| p.manifest.suggestions.clone())
            .expect("fixture plugin declares suggestions"),
    );
    host.with_suggestions_db(|conn, profile| {
        conn.execute(
            "INSERT OR IGNORE INTO suggestion_provider_prefs (profile, provider_id, enabled)
             VALUES (?1, ?2, 1)",
            [profile, PROVIDER],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO suggestion_providers (profile, provider_id, updated_at)
             VALUES (?1, ?2, 0)",
            [profile, PROVIDER],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO suggestion_items (profile, provider_id, item_id, item_json, spec_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            [profile, PROVIDER, id, &item.to_string(), &hash],
        )
        .unwrap();
    });
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
    assert!(
        body["setup_offered"].is_boolean(),
        "the TUI takes the featured index from the daemon"
    );
    assert!(
        body["profile"].is_string(),
        "clients check it against their own"
    );

    for uri in ["/api/suggestions/refresh", "/api/suggestions/checks/run"] {
        let (status, _, body) = f.call(Method::POST, uri, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body, json!({ "ok": true, "notified": 0 }), "{uri}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn an_agent_can_push_a_suggestion_without_a_plugin() {
    let f = fixture();
    let (status, _, body) = f
        .call(
            Method::POST,
            "/api/suggestions/push",
            json!({ "title": "Fix flaky test", "prompt": "Fix it", "reason": "Fails on CI" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    let item = snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .expect("pushed item is listed");
    assert_eq!(item["provider_id"], "builtin.agent");
    assert_eq!(item["draft"]["prompt"], "Fix it");

    let (status, code, _) = f
        .call(
            Method::POST,
            "/api/suggestions/push",
            json!({ "title": " ", "prompt": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "validation_failed");
}

/// `started_session` follows the link table joined to live Sessions, not a scan
/// of Sessions that carry an origin; CityHall keeps terminal Sessions hidden.
#[tokio::test]
#[serial_test::serial]
async fn started_session_comes_from_the_link_joined_to_live_sessions() {
    let origin = || {
        Some(crate::session::SuggestionOrigin {
            provider_id: PROVIDER.into(),
            id: "a".into(),
            profile: None,
        })
    };
    let mut linked = crate::session::Instance::new("terminal", "/tmp/x");
    linked.source_profile = "default".into();
    linked.suggestion = origin();
    let mut trashed = linked.clone();
    trashed.id = "trashed".into();
    trashed.trash();
    let started = |body: &Value| body["items"][0].get("started_session").cloned();
    for (cityhall, shown) in [(false, true), (true, false)] {
        let f = fixture_with(vec![linked.clone()], cityhall);
        f.seed_item("a", None, "42");
        let (_, _, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
        assert_eq!(
            started(&body),
            None,
            "an origin without a link is not started"
        );
        link_session_on(&f.host, "a", &linked.id);
        let (status, _, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(started(&body).is_some(), shown, "cityhall={cityhall}");
    }
    // A link whose Session is gone or trashed shows nothing.
    let f = fixture_with(vec![trashed], false);
    f.seed_item("a", None, "42");
    link_session_on(&f.host, "a", "trashed");
    let (_, _, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(started(&body), None);
    link_session_on(&f.host, "a", "gone");
    let (_, _, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(started(&body), None);
}

#[tokio::test]
#[serial_test::serial]
async fn an_unreadable_project_registry_is_reported_not_hidden() {
    let f = fixture();
    f.seed_item("a", None, "42");
    std::fs::write(
        crate::session::get_app_dir().unwrap().join("projects.json"),
        "not json",
    )
    .unwrap();
    let (status, code, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(code, "internal");
}

#[tokio::test]
#[serial_test::serial]
async fn prepare_reports_an_unreadable_project_registry() {
    let f = fixture();
    f.seed_item("a", None, "42");
    std::fs::write(
        crate::session::get_app_dir().unwrap().join("projects.json"),
        "not json",
    )
    .unwrap();
    let (status, code, body) = f
        .call(
            Method::POST,
            "/api/suggestions/prepare",
            json!({ "provider_id": PROVIDER, "id": "a" }),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(code, "internal");
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

fn preset_body(criterion: &str, agent: &str, prompt: Value) -> Value {
    json!({ "provider_id": PROVIDER, "criterion": criterion, "preset": {
        "agent": agent, "view": "terminal", "worktree": false, "sandbox": true,
        "yolo": true, "prompt": prompt } })
}

/// The snapshot carries each kind's preset and what Start uses for each item.
#[tokio::test]
#[serial_test::serial]
async fn the_snapshot_shows_each_kinds_preset_and_what_start_uses() {
    let f = fixture();
    f.seed_item("a", Some("acme/web"), "42");
    let snapshot = || async {
        let (status, _, body) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        body
    };
    let body = snapshot().await;
    let source = &body["providers"][0]["sources"][0];
    assert_eq!(source["criterion"], "assigned");
    assert_eq!(source["customized"], false);
    let default_agent = source["preset"]["agent"].clone();
    assert_eq!(body["items"][0]["start"]["agent"], default_agent);
    assert!(
        body["items"][0]["start"].get("project").is_none(),
        "unlinked"
    );
    assert!(!body["items"][0]["start"]["yolo"].is_null());
    assert_eq!(
        body["items"][0]["start"]["prompt"], "Fix #{number}",
        "the published prompt"
    );

    f.call(
        Method::PUT,
        "/api/suggestions/presets",
        preset_body("assigned", "codex", Value::Null),
    )
    .await;
    // Linked, the item names the Project Start would use.
    let repo = f.repo.to_string_lossy().into_owned();
    f.call(
        Method::PUT,
        "/api/suggestions/mappings",
        json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo] }),
    )
    .await;
    let body = snapshot().await;
    let source = &body["providers"][0]["sources"][0];
    assert_eq!(source["customized"], true);
    assert_eq!(source["preset"]["agent"], "codex");
    let start = &body["items"][0]["start"];
    assert_eq!(start["agent"], "codex");
    assert_eq!(
        start["project"]["path"],
        body["items"][0]["resolved_project"]["path"]
    );
    for field in ["view", "worktree", "sandbox"] {
        assert!(!start[field].is_null(), "{field}");
    }
    assert_eq!(start["yolo"], true, "the preset auto-approves");

    f.call(
        Method::PUT,
        "/api/suggestions/presets",
        preset_body("assigned", "codex", json!("Look at #{number}")),
    )
    .await;
    assert_eq!(
        snapshot().await["items"][0]["start"]["prompt"],
        "Look at #42",
        "the preset template, expanded"
    );
}

/// The snapshot's start.prompt is what prepare reports and start sends: blank
/// stays empty, and an expansion over the limit is absent where both refuse.
#[tokio::test]
#[serial_test::serial]
async fn the_snapshot_prompt_agrees_with_prepare_for_blank_and_oversize() {
    let f = fixture();
    f.seed_item("a", None, "42");
    f.seed_item("big", None, &"x".repeat(1000));
    let big_template = "{number}".repeat(100);
    for template in ["   ", big_template.as_str()] {
        f.call(
            Method::PUT,
            "/api/suggestions/presets",
            preset_body("assigned", "codex", json!(template)),
        )
        .await;
        let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
        for id in ["a", "big"] {
            let shown = snapshot["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id)
                .unwrap()["start"]
                .get("prompt")
                .cloned();
            let (status, _, prepared) = f
                .call(
                    Method::POST,
                    "/api/suggestions/prepare",
                    json!({ "provider_id": PROVIDER, "id": id }),
                )
                .await;
            match shown {
                Some(prompt) => {
                    assert_eq!(status, StatusCode::OK, "{template:?} {id}");
                    assert_eq!(prompt, prepared["initial_prompt"], "{template:?} {id}");
                }
                None => {
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{template:?} {id}");
                    assert_eq!(id, "big");
                }
            }
        }
        if template == "   " {
            assert_eq!(snapshot["items"][0]["start"]["prompt"], "");
        }
    }
}

/// A request that names another profile than the one this Daemon serves is
/// refused before it changes anything; the served profile or no field is
/// handled as before.
#[tokio::test]
#[serial_test::serial]
async fn a_suggestion_request_for_another_profile_is_refused_untouched() {
    let f = fixture();
    f.seed_item("a", Some("acme/web"), "42");
    let item = |extra: Value| {
        let mut body = json!({ "provider_id": PROVIDER, "id": "a" });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let other = json!({ "expected_profile": "other" });
    let repo = f.repo.to_string_lossy().into_owned();
    let key = "provider_id=acme.gh";
    for (method, uri, body) in [
        (
            Method::POST,
            "/api/suggestions/dismiss".to_string(),
            item(other.clone()),
        ),
        (
            Method::POST,
            "/api/suggestions/undismiss".to_string(),
            item(other.clone()),
        ),
        (
            Method::POST,
            "/api/suggestions/prepare".to_string(),
            item(other.clone()),
        ),
        (
            Method::POST,
            "/api/suggestions/start".to_string(),
            item(other.clone()),
        ),
        (
            Method::POST,
            "/api/suggestions/start-batch".to_string(),
            json!({ "items": [{ "provider_id": PROVIDER, "id": "a" }], "expected_profile": "other" }),
        ),
        (
            Method::POST,
            "/api/suggestions/refresh".to_string(),
            other.clone(),
        ),
        (
            Method::POST,
            "/api/suggestions/checks/run".to_string(),
            other.clone(),
        ),
        (
            Method::POST,
            "/api/suggestions/fix".to_string(),
            json!({ "provider_id": PROVIDER, "expected_profile": "other" }),
        ),
        (
            Method::PATCH,
            "/api/suggestions/sources".to_string(),
            json!({ "provider_id": PROVIDER, "criterion": "assigned", "enabled": false, "expected_profile": "other" }),
        ),
        (
            Method::PATCH,
            "/api/suggestions/prefs".to_string(),
            json!({ "refresh_minutes": 30, "expected_profile": "other" }),
        ),
        (Method::PUT, "/api/suggestions/presets".to_string(), {
            let mut body = preset_body("assigned", "codex", Value::Null);
            body["expected_profile"] = json!("other");
            body
        }),
        (
            Method::DELETE,
            format!("/api/suggestions/presets?{key}&criterion=assigned&expected_profile=other"),
            Value::Null,
        ),
        (
            Method::PUT,
            "/api/suggestions/mappings".to_string(),
            json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo], "expected_profile": "other" }),
        ),
        (
            Method::DELETE,
            format!("/api/suggestions/mappings?{key}&mapping_key=acme/web&expected_profile=other"),
            Value::Null,
        ),
    ] {
        let (status, code, answer) = f.call(method, &uri, body).await;
        assert_eq!(
            (status, code.as_str()),
            (StatusCode::CONFLICT, "profile_mismatch"),
            "{uri}"
        );
        assert!(
            answer
                .to_string()
                .contains(r#"serves profile default, not"#),
            "{answer}"
        );
    }
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["items"][0]["dismissed"], false, "{snapshot}");
    assert_eq!(snapshot["prefs"]["refresh_minutes"], 15);
    assert_eq!(snapshot["providers"][0]["sources"][0]["enabled"], true);
    assert_eq!(snapshot["providers"][0]["sources"][0]["customized"], false);
    assert!(snapshot["items"][0]["start"].get("project").is_none());

    // The served profile, or none, is accepted.
    for body in [
        item(json!({ "expected_profile": "default" })),
        item(json!({})),
    ] {
        let (status, _, _) = f.call(Method::POST, "/api/suggestions/dismiss", body).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = f
            .call(Method::POST, "/api/suggestions/undismiss", item(json!({})))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _, _) = f
        .call(
            Method::POST,
            "/api/suggestions/refresh",
            json!({ "expected_profile": "default" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// Refresh and Check are sent with a JSON content type and no body: that is no
/// guard. A body that is not a guard is a 400.
#[tokio::test]
#[serial_test::serial]
async fn an_empty_json_body_is_accepted_as_no_guard() {
    let f = fixture();
    let send = |uri: &'static str, body: &'static str| {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            crate::server::peer::ConnectionPeer::UnixOwner {
                uid: nix::unistd::geteuid().as_raw(),
            },
        ));
        let app = f.app.clone();
        async move { app.oneshot(request).await.unwrap().status() }
    };
    for uri in ["/api/suggestions/refresh", "/api/suggestions/checks/run"] {
        for body in ["", "  ", "null", "{}"] {
            assert_eq!(send(uri, body).await, StatusCode::OK, "{uri} {body:?}");
        }
        for body in ["[1]", r#"{"expected_profil":"x"}"#] {
            assert_eq!(
                send(uri, body).await,
                StatusCode::BAD_REQUEST,
                "{uri} {body}"
            );
        }
    }
}

/// An installed provider with no stored switch is listed as off and shows
/// nothing; the API turns it on.
#[tokio::test]
#[serial_test::serial]
async fn a_provider_is_listed_off_until_the_api_switches_it_on() {
    let f = fixture();
    f.seed_item("a", None, "42");
    f.host.with_suggestions_db(|conn, _| {
        conn.execute("DELETE FROM suggestion_provider_prefs", [])
            .unwrap();
    });
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["providers"][0]["id"], PROVIDER);
    assert_eq!(snapshot["providers"][0]["enabled"], false);
    assert_eq!(snapshot["items"], json!([]));

    let (status, _, _) = f
        .call(
            Method::PATCH,
            "/api/suggestions/prefs",
            json!({ "provider_enabled": { PROVIDER: true } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(snapshot["providers"][0]["enabled"], true);
    assert_eq!(snapshot["items"][0]["id"], "a");
}

#[tokio::test]
#[serial_test::serial]
async fn presets_default_round_trip_reset_and_validate() {
    let f = fixture();
    let entry = |body: &Value| {
        body["presets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["criterion"] == "assigned")
            .unwrap()
            .clone()
    };

    let (status, _, body) = f
        .call(Method::GET, "/api/suggestions/presets", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let assigned = entry(&body);
    assert_eq!(assigned["customized"], false);
    assert_eq!(assigned["preset"], assigned["defaults"]);
    assert_eq!(assigned["defaults"]["prompt"], Value::Null);

    let (status, _, put) = f
        .call(
            Method::PUT,
            "/api/suggestions/presets",
            preset_body("assigned", "codex", json!("Look at {number}")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!(put["customized"], true);
    assert_eq!(put["preset"]["agent"], "codex");
    let (_, _, body) = f
        .call(Method::GET, "/api/suggestions/presets", Value::Null)
        .await;
    let assigned = entry(&body);
    assert_eq!(assigned["preset"]["prompt"], "Look at {number}");
    assert_ne!(assigned["preset"], assigned["defaults"]);

    for (body, code) in [
        (
            preset_body("undeclared", "codex", json!("x")),
            "validation_failed",
        ),
        (
            preset_body("assigned", "no-such-agent", json!("x")),
            "validation_failed",
        ),
        (
            preset_body("assigned", "codex", json!("x".repeat(65 * 1024))),
            "validation_failed",
        ),
    ] {
        let (status, got, _) = f.call(Method::PUT, "/api/suggestions/presets", body).await;
        assert_eq!((status, got.as_str()), (StatusCode::BAD_REQUEST, code));
    }

    let query = format!("/api/suggestions/presets?provider_id={PROVIDER}&criterion=assigned");
    let (status, _, _) = f.call(Method::DELETE, &query, Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = f
        .call(
            Method::DELETE,
            &format!("/api/suggestions/presets?provider_id={PROVIDER}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, _, body) = f
        .call(Method::GET, "/api/suggestions/presets", Value::Null)
        .await;
    assert_eq!(entry(&body)["customized"], false);
}

#[tokio::test]
#[serial_test::serial]
async fn prepare_resolves_the_preset_prompt_and_stale_items() {
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
    // Only a preset prompt expands variables; the draft prompt is used as published.
    assert_eq!(body["initial_prompt"], "Fix #{number}");
    assert_eq!(body["source_ref"], "acme/web#42");
    assert_eq!(body["project"], Value::Null);

    f.call(
        Method::PUT,
        "/api/suggestions/presets",
        preset_body("assigned", "claude", json!("Review {number}")),
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
        "/api/suggestions/presets",
        preset_body("assigned", "claude", json!("{number}".repeat(100))),
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

    // An item its source closed is refused the same way as one no longer published.
    f.seed_item("closed", None, "7");
    f.host.with_suggestions_db(|conn, _| {
        conn.execute(
            "UPDATE suggestion_items SET item_json = json_set(item_json, '$.stale', json('true'))
             WHERE item_id = 'closed'",
            [],
        )
        .unwrap();
    });
    for (uri, body, status, code) in [
        (
            "prepare",
            item(PROVIDER, "gone"),
            StatusCode::CONFLICT,
            "stale_suggestion",
        ),
        (
            "prepare",
            item(PROVIDER, "closed"),
            StatusCode::CONFLICT,
            "stale_suggestion",
        ),
        (
            "start",
            item(PROVIDER, "closed"),
            StatusCode::CONFLICT,
            "stale_suggestion",
        ),
        (
            "prepare",
            item("nobody", "a"),
            StatusCode::NOT_FOUND,
            "unknown_provider",
        ),
    ] {
        let uri = format!("/api/suggestions/{uri}");
        let (got, got_code, _) = f.call(Method::POST, &uri, body).await;
        assert_eq!((got, got_code.as_str()), (status, code), "{uri}");
    }
}

/// Start in a Project moves it to the front of the repo's mapping and keeps
/// the other Projects the user had mapped.
#[tokio::test]
#[serial_test::serial]
async fn start_in_a_project_keeps_the_other_mapped_projects() {
    let f = fixture();
    f.seed_item("a", Some("acme/web"), "42");
    let other = f.repo.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).unwrap();
    projects::add(
        "default",
        ProjectScope::Global,
        Project::new("other", other.to_string_lossy(), ProjectScope::Global),
        false,
    )
    .unwrap();
    let canonical = |path: &str| {
        std::fs::canonicalize(path)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    let (repo, other) = (
        f.repo.to_string_lossy().into_owned(),
        other.to_string_lossy().into_owned(),
    );
    f.call(
        Method::PUT,
        "/api/suggestions/mappings",
        json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [other, repo] }),
    )
    .await;
    // The hooks gate ends the start after the link is saved.
    std::fs::create_dir_all(f.repo.join(".agent-of-empires")).unwrap();
    std::fs::write(
        f.repo.join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"npm ci\"]\n",
    )
    .unwrap();
    let (status, code, _) = f
        .call(
            Method::POST,
            "/api/suggestions/start",
            json!({ "provider_id": PROVIDER, "id": "a", "project": { "path": repo } }),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::CONFLICT, "hooks_need_trust")
    );
    let (_, _, mappings) = f
        .call(Method::GET, "/api/suggestions/mappings", Value::Null)
        .await;
    assert_eq!(
        mappings["mappings"][0]["projects"],
        json!([canonical(&repo), canonical(&other)]),
        "{mappings}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn start_refuses_an_unlinked_repo_and_shows_hooks_before_approval() {
    let f = fixture();
    let start = |extra: Value| {
        let mut body = json!({ "provider_id": PROVIDER, "id": "a" });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    f.seed_item("a", Some("acme/web"), "42");

    let (status, code, body) = f
        .call(Method::POST, "/api/suggestions/start", start(json!({})))
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "no_project")
    );
    assert_eq!(body["hint"], "link_project");

    // Start in...: a path that is not a registered Project saves nothing.
    let (status, code, _) = f
        .call(
            Method::POST,
            "/api/suggestions/start",
            start(json!({ "project": { "path": "/not/registered" } })),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );
    let (_, _, mappings) = f
        .call(Method::GET, "/api/suggestions/mappings", Value::Null)
        .await;
    assert_eq!(mappings["mappings"], json!([]), "{mappings}");

    // The repo's hooks must be reviewed first; nothing is created.
    std::fs::create_dir_all(f.repo.join(".agent-of-empires")).unwrap();
    std::fs::write(
        f.repo.join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"npm ci\"]\n",
    )
    .unwrap();
    let repo = f.repo.to_string_lossy().into_owned();
    let (status, code, body) = f
        .call(
            Method::POST,
            "/api/suggestions/start",
            start(json!({ "project": { "path": repo } })),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::CONFLICT, "hooks_need_trust"),
        "{body}"
    );
    assert_eq!(body["commands"]["on_create"], json!(["npm ci"]));
    assert!(body["review_fingerprint"]["project_path"].is_string());
    // The link was saved, so approving needs no project again.
    let (_, _, mappings) = f
        .call(Method::GET, "/api/suggestions/mappings", Value::Null)
        .await;
    assert_eq!(
        mappings["mappings"][0]["mapping_key"], "acme/web",
        "{mappings}"
    );

    let (status, code, _) = f
        .call(
            Method::POST,
            "/api/suggestions/start",
            start(json!({ "approve_hooks": true })),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );
}

#[tokio::test]
#[serial_test::serial]
async fn start_batch_answers_each_item_and_one_refusal_never_stops_the_rest() {
    let f = fixture();
    f.seed_item("hooks", Some("acme/web"), "1");
    f.seed_item("unlinked", Some("acme/other"), "2");
    let repo = f.repo.to_string_lossy().into_owned();
    f.call(
        Method::PUT,
        "/api/suggestions/mappings",
        json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo] }),
    )
    .await;
    std::fs::create_dir_all(f.repo.join(".agent-of-empires")).unwrap();
    std::fs::write(
        f.repo.join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"npm ci\"]\n",
    )
    .unwrap();
    let item = |id: &str| json!({ "provider_id": PROVIDER, "id": id });
    let (status, _, body) = f
        .call(
            Method::POST,
            "/api/suggestions/start-batch",
            json!({ "items": [item("hooks"), item("unlinked"), item("gone")] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let results: Vec<(String, u64, Value)> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["status"].as_u64().unwrap(),
                r["body"]["error"].clone(),
            )
        })
        .collect();
    assert_eq!(
        results,
        [
            ("hooks".to_string(), 409, json!("hooks_need_trust")),
            ("unlinked".to_string(), 400, json!("no_project")),
            ("gone".to_string(), 409, json!("stale_suggestion")),
        ]
    );

    for items in [json!([]), json!(vec![item("x"); 26])] {
        let (status, code, _) = f
            .call(
                Method::POST,
                "/api/suggestions/start-batch",
                json!({ "items": items }),
            )
            .await;
        assert_eq!(
            (status, code.as_str()),
            (StatusCode::BAD_REQUEST, "validation_failed")
        );
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

#[tokio::test]
#[serial_test::serial]
async fn projects_list_each_registered_project_with_its_origin_slug() {
    let f = fixture();
    let cloned = f.repo.parent().unwrap().join("cloned");
    git2::Repository::init(&cloned)
        .unwrap()
        .remote("origin", "git@github.com:Acme/Web.git")
        .unwrap();
    projects::add(
        "default",
        ProjectScope::Global,
        Project::new("web-app", cloned.to_string_lossy(), ProjectScope::Global),
        false,
    )
    .unwrap();

    let (status, _, body) = f
        .call(Method::GET, "/api/suggestions/projects", Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let rows: Vec<(&str, Value)> = body["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap(), p["origin"].clone()))
        .collect();
    assert_eq!(
        rows,
        [("repo", Value::Null), ("web-app", json!("Acme/Web"))]
    );
}

fn items_by_id(snapshot: &Value) -> std::collections::HashMap<String, Value> {
    snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["id"].as_str().unwrap().to_string(), item.clone()))
        .collect()
}

#[tokio::test]
#[serial_test::serial]
async fn auto_start_is_validated_and_leaves_what_needs_the_user_with_a_note() {
    let f = fixture();
    let put = |body: Value| f.call(Method::PUT, "/api/suggestions/auto-start", body);
    let on = json!({ "provider_id": PROVIDER, "criterion": "assigned", "enabled": true });
    for (what, mut body) in [
        ("cap 0", on.clone()),
        ("cap 11", on.clone()),
        ("unknown kind", on.clone()),
    ] {
        match what {
            "cap 0" => body["cap"] = json!(0),
            "cap 11" => body["cap"] = json!(11),
            _ => body["criterion"] = json!("nope"),
        }
        let (status, code, _) = put(body).await;
        assert_eq!(
            (status, code.as_str()),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{what}"
        );
    }
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(
        snapshot["providers"][0]["sources"][0]["auto_start"],
        Value::Null
    );

    let (status, _, body) = put(on).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["auto_start"], json!({ "cap": 3 }));

    // No project is linked to the repo: it stays a suggestion and says why.
    f.seed_item("a", Some("acme/web"), "42");
    super::suggestions::sweep_auto_start(&f.state).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert_eq!(
        snapshot["providers"][0]["sources"][0]["auto_start"],
        json!({ "cap": 3 })
    );
    let note = items_by_id(&snapshot)["a"]["auto_note"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        note.starts_with("Not auto started: ") && note.contains("not linked"),
        "{note}"
    );

    // Linked, but the repo's hooks need a review.
    let repo = f.repo.to_string_lossy().into_owned();
    f.call(
        Method::PUT,
        "/api/suggestions/mappings",
        json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo] }),
    )
    .await;
    std::fs::create_dir_all(f.repo.join(".agent-of-empires")).unwrap();
    std::fs::write(
        f.repo.join(".agent-of-empires/config.toml"),
        "[hooks]\non_create = [\"npm ci\"]\n",
    )
    .unwrap();
    super::suggestions::sweep_auto_start(&f.state).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    let note = items_by_id(&snapshot)["a"]["auto_note"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(note.contains("hooks"), "{note}");
    let attempted: i64 = f.host.with_suggestions_db(|conn, _| {
        conn.query_row("SELECT COUNT(*) FROM suggestion_auto_started", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert_eq!(
        attempted, 0,
        "an item that needs the user is never attempted"
    );

    // Turned off, the note goes away.
    put(json!({ "provider_id": PROVIDER, "criterion": "assigned", "enabled": false })).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    assert!(items_by_id(&snapshot)["a"].get("auto_note").is_none());
}

#[tokio::test]
#[serial_test::serial]
async fn auto_start_is_refused_for_the_agent_provider() {
    let f = fixture();
    let agent = crate::plugin::suggestions::AGENT_PROVIDER_ID;
    let (status, code, _) = f
        .call(
            Method::PUT,
            "/api/suggestions/auto-start",
            json!({ "provider_id": agent, "criterion": "agent", "enabled": true }),
        )
        .await;
    assert_eq!(
        (status, code.as_str()),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );
    // A stored switch is ignored by the sweep: the item gets no note at all.
    f.host.with_suggestions_db(|conn, profile| {
        crate::plugin::suggestions::set_auto_start(conn, profile, agent, "agent", Some(3)).unwrap();
    });
    let (status, _, _) = f
        .call(
            Method::POST,
            "/api/suggestions/push",
            json!({ "title": "Look", "prompt": "do it" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    super::suggestions::sweep_auto_start(&f.state).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    let items = snapshot["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].get("auto_note").is_none(), "{items:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn auto_start_waits_for_the_provider_switch_and_stays_out_of_cityhall() {
    for (cityhall, switch_off) in [(false, true), (true, false)] {
        let f = fixture_with(Vec::new(), cityhall);
        f.seed_item("a", Some("acme/web"), "42");
        let repo = f.repo.to_string_lossy().into_owned();
        f.call(
            Method::PUT,
            "/api/suggestions/mappings",
            json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo] }),
        )
        .await;
        f.host.with_suggestions_db(|conn, profile| {
            crate::plugin::suggestions::set_auto_start(
                conn,
                profile,
                PROVIDER,
                "assigned",
                Some(3),
            )
            .unwrap();
            if switch_off {
                conn.execute("UPDATE suggestion_provider_prefs SET enabled = 0", [])
                    .unwrap();
            }
        });
        super::suggestions::sweep_auto_start(&f.state).await;
        let attempted: i64 = f.host.with_suggestions_db(|conn, _| {
            conn.query_row("SELECT COUNT(*) FROM suggestion_auto_started", [], |r| {
                r.get(0)
            })
            .unwrap()
        });
        assert_eq!(attempted, 0, "cityhall={cityhall} switch_off={switch_off}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn auto_start_never_starts_a_yolo_preset() {
    let f = fixture();
    f.seed_item("a", Some("acme/web"), "42");
    let repo = f.repo.to_string_lossy().into_owned();
    f.call(
        Method::PUT,
        "/api/suggestions/mappings",
        json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo] }),
    )
    .await;
    f.host.with_suggestions_db(|conn, profile| {
        let mut preset = crate::plugin::suggestions::default_preset(
            &crate::session::config::profile_config::resolve_config_or_warn(profile),
        );
        preset.yolo = true;
        crate::plugin::suggestions::set_preset(conn, profile, PROVIDER, "assigned", &preset)
            .unwrap();
    });
    f.call(
        Method::PUT,
        "/api/suggestions/auto-start",
        json!({ "provider_id": PROVIDER, "criterion": "assigned", "enabled": true }),
    )
    .await;
    super::suggestions::sweep_auto_start(&f.state).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    let note = items_by_id(&snapshot)["a"]["auto_note"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(note.contains("YOLO"), "{note}");
    let attempted: i64 = f.host.with_suggestions_db(|conn, _| {
        conn.query_row("SELECT COUNT(*) FROM suggestion_auto_started", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert_eq!(attempted, 0);
}

#[tokio::test]
#[serial_test::serial]
async fn auto_start_attempts_each_item_once_and_stops_at_the_cap() {
    let f = fixture();
    for id in ["a", "b", "c"] {
        f.seed_item(id, Some("acme/web"), "42");
    }
    let repo = f.repo.to_string_lossy().into_owned();
    f.call(
        Method::PUT,
        "/api/suggestions/mappings",
        json!({ "provider_id": PROVIDER, "mapping_key": "acme/web", "projects": [repo] }),
    )
    .await;
    // An agent that does not exist makes every create fail the same way.
    f.host.with_suggestions_db(|conn, profile| {
        let mut preset = crate::plugin::suggestions::default_preset(
            &crate::session::config::profile_config::resolve_config_or_warn(profile),
        );
        preset.agent = "no-such-agent".into();
        crate::plugin::suggestions::set_preset(conn, profile, PROVIDER, "assigned", &preset)
            .unwrap();
    });
    f.call(
        Method::PUT,
        "/api/suggestions/auto-start",
        json!({ "provider_id": PROVIDER, "criterion": "assigned", "enabled": true, "cap": 2 }),
    )
    .await;
    let notes = |snapshot: &Value| -> Vec<String> {
        let mut notes: Vec<String> = items_by_id(snapshot)
            .values()
            .map(|item| item["auto_note"].as_str().unwrap_or_default().to_string())
            .collect();
        notes.sort();
        notes
    };

    super::suggestions::sweep_auto_start(&f.state).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    let notes_now = notes(&snapshot);
    let failed = notes_now
        .iter()
        .filter(|n| n.starts_with("Auto start failed: "))
        .count();
    let capped = notes_now
        .iter()
        .filter(|n| n.contains("limit of 2"))
        .count();
    assert_eq!((failed, capped), (2, 1), "{notes_now:?}");
    assert!(notes_now
        .iter()
        .all(|n| n.is_empty() || n.ends_with("Start it by hand.") || n.contains("limit")));

    // The next pass takes the one the cap held back, and never retries a failure.
    super::suggestions::sweep_auto_start(&f.state).await;
    let (_, _, snapshot) = f.call(Method::GET, "/api/suggestions", Value::Null).await;
    let notes_now = notes(&snapshot);
    let failed = notes_now
        .iter()
        .filter(|n| n.starts_with("Auto start failed: "))
        .count();
    assert_eq!(failed, 3, "{notes_now:?}");
    let attempted: i64 = f.host.with_suggestions_db(|conn, _| {
        conn.query_row("SELECT COUNT(*) FROM suggestion_auto_started", [], |r| {
            r.get(0)
        })
        .unwrap()
    });
    assert_eq!(attempted, 3);
}
