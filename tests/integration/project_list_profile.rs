//! `GET /api/projects?profile=` resolves the registry for the requested profile rather than the
//! served one, so a wizard that switches profile reads that profile's per-project overrides.

use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state, refresh_canonical_metadata_for_test,
};
use agent_of_empires::session::projects;
use agent_of_empires::session::{Project, ProjectOverrides, ProjectScope};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("host", "127.0.0.1")
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = "127.0.0.1:5558".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
#[serial_test::serial]
async fn list_projects_resolves_overrides_for_the_requested_profile() {
    let home = crate::common::setup_temp_home();
    let repo = home.path().join("demo");
    std::fs::create_dir_all(&repo).unwrap();

    for profile in ["test", "a", "b"] {
        agent_of_empires::session::create_profile(profile).unwrap();
    }
    projects::add(
        "test",
        ProjectScope::Global,
        Project::new("demo", repo.to_string_lossy(), ProjectScope::Global).with_overrides(
            ProjectOverrides {
                sandbox_enabled: Some(true),
                ..Default::default()
            },
        ),
        false,
    )
    .unwrap();
    for (profile, sandbox) in [("a", false), ("b", true)] {
        let project = Project::new("demo", repo.to_string_lossy(), ProjectScope::Profile)
            .with_overrides(ProjectOverrides {
                sandbox_enabled: Some(sandbox),
                ..Default::default()
            });
        projects::add(profile, ProjectScope::Profile, project, true).unwrap();
    }

    let state = build_test_app_state(Vec::new());
    refresh_canonical_metadata_for_test(&state).await;
    let app = build_router_for_test(state);
    for (profile, sandbox) in [("a", false), ("b", true)] {
        let (status, body) = get(&app, &format!("/api/projects?profile={profile}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body[0]["overrides"]["sandbox_enabled"], sandbox,
            "profile {profile}: {body}"
        );
    }

    let (status, body) = get(&app, "/api/projects?profile=../escape").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "bad_profile", "{body}");
    for uri in [
        "/api/projects",
        "/api/projects?scope=global",
        "/api/projects?scope=profile&profile=b",
    ] {
        let (status, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body[0]["overrides"]["sandbox_enabled"], true, "{body}");
    }
}

/// Registry reads remain on the last complete native publication until a disk reload commits.
#[tokio::test]
#[serial_test::serial]
async fn list_projects_retains_committed_registry_and_distinguishes_unknown_profiles() {
    let home = crate::common::setup_temp_home();
    let repo = home.path().join("demo");
    std::fs::create_dir_all(&repo).unwrap();
    for profile in ["test", "a", "b", "empty"] {
        agent_of_empires::session::create_profile(profile).unwrap();
    }
    let project = Project::new("demo", repo.to_string_lossy(), ProjectScope::Profile)
        .with_overrides(ProjectOverrides {
            sandbox_enabled: Some(true),
            ..Default::default()
        });
    projects::add("a", ProjectScope::Profile, project, false).unwrap();
    projects::add(
        "b",
        ProjectScope::Profile,
        Project::new("other", repo.to_string_lossy(), ProjectScope::Profile),
        false,
    )
    .unwrap();

    let state = build_test_app_state(Vec::new());
    refresh_canonical_metadata_for_test(&state).await;
    let app = build_router_for_test(state);
    let (status, body) = get(&app, "/api/projects?profile=a").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, committed) = get(&app, "/api/projects?profile=b").await;
    assert_eq!(status, StatusCode::OK, "{committed}");
    let broken = agent_of_empires::session::get_profile_dir_path("b")
        .unwrap()
        .join("projects.json");
    std::fs::write(&broken, "{ not json").unwrap();
    let (status, body) = get(&app, "/api/projects?profile=b").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, committed);
    assert!(projects::load_merged("b").is_err());

    let (status, body) = get(&app, "/api/projects?profile=empty").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, serde_json::json!([]));
    let (status, body) = get(&app, "/api/projects?profile=never-registered").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "profile_not_found");
}
