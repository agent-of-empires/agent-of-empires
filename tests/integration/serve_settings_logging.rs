use agent_of_empires::logging;
use agent_of_empires::server::test_support::{build_router_for_test, build_test_app_state};
use agent_of_empires::session::{self, Config};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tower::ServiceExt;

struct RestoreFilter(String);

impl Drop for RestoreFilter {
    fn drop(&mut self) {
        logging::set_filter(&self.0).expect("restore runtime filter");
    }
}

async fn patch(app: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    let mut request = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap()));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        expected,
        "{uri}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn settings_only_reload_changed_log_filters() {
    let _home = crate::common::setup_temp_home();
    if logging::controller().is_none() {
        let init = logging::init_subscriber(logging::SubscriberTarget::Stdout, "off".into());
        logging::install_controller(init.controller.expect("reloadable subscriber"));
    }
    let _restore_filter = RestoreFilter(logging::current_filter().unwrap());
    let app = build_router_for_test(build_test_app_state(Vec::new()));
    let runtime_path = logging::runtime_filter_path(&session::get_app_dir().unwrap());
    let temporary = "agent_of_empires=debug,acp.protocol=trace";
    for uri in ["/api/settings", "/api/profiles/main/settings"] {
        session::update_config(|config| {
            config.logging = Default::default();
            config.logging.default_level = "info".into();
            config
                .logging
                .targets
                .insert("acp.protocol".into(), "warn".into());
        })
        .unwrap();
        logging::set_filter("agent_of_empires=info").unwrap();

        let cases = [
            ("empty patch", json!({}), false),
            ("empty logging", json!({"logging": {}}), false),
            (
                "same baseline",
                json!({"logging": {"default_level": "info"}}),
                false,
            ),
            (
                "same targets",
                json!({"logging": {"targets": {"acp.protocol": "warn"}}}),
                false,
            ),
            (
                "same filter",
                json!({"logging": {"default_level": "info", "targets": {"acp.protocol": "warn"}}}),
                false,
            ),
            (
                "sink settings",
                json!({"logging": {
                    "output": "stdout", "file_path": "server.log", "rotation": "never",
                    "max_size_mib": 17, "keep_count": 3, "show_spans": true
                }}),
                false,
            ),
            (
                "changed baseline",
                json!({"logging": {"default_level": "error"}}),
                true,
            ),
            (
                "changed targets",
                json!({"logging": {"targets": {"acp.protocol": "debug"}}}),
                true,
            ),
            (
                "replaced targets",
                json!({"logging": {"targets": {"server": "warn"}}}),
                true,
            ),
            ("removed targets", json!({"logging": {"targets": {}}}), true),
            (
                "restored targets",
                json!({"logging": {"targets": {"acp.protocol": "warn"}}}),
                true,
            ),
            ("reset targets", json!({"logging": {"targets": null}}), true),
            (
                "already reset targets",
                json!({"logging": {"targets": null}}),
                false,
            ),
        ];
        for (name, body, filter_changed) in cases {
            let runtime = patch(
                &app,
                "/api/log-level",
                json!({"filter": temporary}),
                StatusCode::OK,
            )
            .await;
            assert_eq!(runtime["current"], temporary, "{uri}: {name}");
            let rejected = uri != "/api/settings"
                && body
                    .get("logging")
                    .and_then(Value::as_object)
                    .is_some_and(|fields| !fields.is_empty());
            let before = serde_json::to_value(Config::load().unwrap()).unwrap();
            let response = patch(
                &app,
                uri,
                body.clone(),
                if rejected {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::OK
                },
            )
            .await;
            let saved = Config::load().unwrap();
            let saved_logging = serde_json::to_value(&saved.logging).unwrap();
            if rejected {
                assert_eq!(response["error"], "validation_failed");
                assert_eq!(
                    serde_json::to_value(&saved).unwrap(),
                    before,
                    "{uri}: {name}"
                );
            } else if uri == "/api/settings" {
                assert_eq!(response["logging"], saved_logging, "{uri}: {name}");
            } else {
                assert!(
                    response.get("logging").is_none(),
                    "logging must remain global"
                );
            }
            if !rejected {
                if let Some(fields) = body.get("logging").and_then(Value::as_object) {
                    for (key, value) in fields {
                        if key == "targets" && value.is_null() {
                            assert_eq!(saved_logging[key], json!({}), "{uri}: {name}: {key}");
                        } else {
                            assert_eq!(&saved_logging[key], value, "{uri}: {name}: {key}");
                        }
                    }
                }
            }
            let expected = if filter_changed && !rejected {
                logging::build_filter_from_config(
                    &saved.logging.default_level,
                    &saved.logging.targets,
                )
                .unwrap()
            } else {
                temporary.to_string()
            };
            assert_eq!(
                logging::current_filter().as_deref(),
                Some(expected.as_str()),
                "{uri}: {name}"
            );
            assert_eq!(
                std::fs::read_to_string(&runtime_path).unwrap().trim(),
                expected,
                "{uri}: {name}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn profile_settings_reject_global_only_fields_without_writing() {
    let _home = crate::common::setup_temp_home();
    session::update_config(|config| *config = Config::default()).unwrap();
    session::save_profile_config("main", &session::ProfileConfig::default()).unwrap();
    let app = build_router_for_test(build_test_app_state(Vec::new()));
    let global_path = session::get_app_dir().unwrap().join("config.toml");
    let profile_path = session::get_profile_dir("main")
        .unwrap()
        .join("config.toml");
    let global_before = std::fs::read(&global_path).unwrap();
    let profile_before = std::fs::read(&profile_path).unwrap();
    let uri = "/api/profiles/main/settings";
    for (section, field, value) in [
        ("theme", "name", json!("dracula")),
        ("theme", "color_mode", json!("palette")),
        ("session", "confirm_before_quit", json!(false)),
        ("session", "sidebar_position", json!("right")),
        ("session", "session_id_poller_max_threads", json!(12)),
        ("web", "notify_on_idle", json!(true)),
        ("logging", "targets", json!({})),
        ("acp", "allow_agent_install", json!(true)),
        ("tmux", "socket_name", json!("custom")),
        ("telemetry", "enabled", json!(true)),
        ("session", "sidebar_position", Value::Null),
    ] {
        let response = patch(
            &app,
            uri,
            json!({"description": "must not save", section: {field: value}}),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(response["error"], "validation_failed");
        let message = response["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("{section}.{field}")),
            "{response}"
        );
        assert!(message.contains("PATCH /api/settings"), "{response}");
        assert_eq!(std::fs::read(&global_path).unwrap(), global_before);
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
    }
    let saved = patch(&app, uri, json!({"description": "work", "theme": {"idle_decay_minutes": 5}, "session": {"default_tool": "codex"}}), StatusCode::OK).await;
    assert_eq!(saved["description"], "work");
    assert_eq!(saved["theme"]["idle_decay_minutes"], 5);
    assert_eq!(saved["session"]["default_tool"], "codex");
    assert_eq!(std::fs::read(&global_path).unwrap(), global_before);
    let saved = patch(
        &app,
        uri,
        json!({"theme": {"idle_decay_minutes": null}}),
        StatusCode::OK,
    )
    .await;
    assert!(saved.get("theme").is_none());
}
