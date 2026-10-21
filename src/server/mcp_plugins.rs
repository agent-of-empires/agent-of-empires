//! Daemon endpoints `/mcp/plugins/{plugin}/{server}` behind which plugins serve MCP servers.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::StatusCode;

use super::auth::AuthenticatedPluginCaller;
use super::state::AppState;

/// The route service for `/mcp/plugins/{plugin}/{server}`: the MCP transport answers every
/// method itself.
pub(super) fn service(state: Arc<AppState>) -> axum::routing::MethodRouter {
    axum::routing::any(plugin_endpoint).with_state(state)
}

/// Placeholder until the plugin MCP handler lands; authentication already applies. Auth only
/// stamps a caller on a well-formed path, so a request without one (e.g. an empty segment,
/// which the router still matches) is not an endpoint.
pub(super) async fn plugin_endpoint(
    caller: Option<Extension<AuthenticatedPluginCaller>>,
) -> StatusCode {
    match caller {
        Some(_) => StatusCode::NO_CONTENT,
        None => StatusCode::NOT_FOUND,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::Extension;
    use axum::http::{header, Request, StatusCode};
    use axum::routing::any;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::server::auth::{auth_middleware, AuthenticatedPluginCaller};
    use crate::server::session_credential::McpSessionSecret;
    use crate::server::test_helpers::vecs;
    use crate::server::{test_support, AppState};
    use crate::session::Instance;

    const TOKEN: &str = "plugin-endpoint-test-token";
    const ENDPOINT: &str = "/mcp/plugins/plug/srv";

    /// Reports the stamped caller; no caller (a malformed path the router still matched) is a 404
    /// like the real handler.
    async fn probe(
        caller: Option<Extension<AuthenticatedPluginCaller>>,
    ) -> (StatusCode, axum::Json<Value>) {
        match caller {
            Some(Extension(c)) => (
                StatusCode::OK,
                axum::Json(json!({
                    "plugin": c.plugin,
                    "server": c.server,
                    "session": c.session,
                })),
            ),
            None => (StatusCode::NOT_FOUND, axum::Json(Value::Null)),
        }
    }

    fn build_state(
        prior: Vec<Instance>,
        token: Option<&str>,
    ) -> (Arc<AppState>, Arc<McpSessionSecret>) {
        build_state_with(prior, token, |_| {})
    }

    /// [`build_state`], with `configure` applied before the state is shared.
    fn build_state_with(
        prior: Vec<Instance>,
        token: Option<&str>,
        configure: impl FnOnce(&mut AppState),
    ) -> (Arc<AppState>, Arc<McpSessionSecret>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = Arc::new(
            McpSessionSecret::load_or_generate(&dir.path().join("mcp_session_secret"))
                .expect("load_or_generate"),
        );
        let state = test_support::build_test_app_state_with_policy_configured(
            prior,
            vecs(&["localhost"]),
            Vec::new(),
            token.map(str::to_string),
            |state| {
                state.web_config.mcp_enabled = true;
                state.mcp_session_secret = Some(secret.clone());
                configure(state);
            },
        );
        (state, secret)
    }

    /// Only the auth middleware in front of probe handlers, so the request extension is visible.
    fn probe_router(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/mcp/plugins/{plugin}/{server}", any(probe))
            .route("/mcp", any(probe))
            .route("/api/sessions", any(probe))
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state)
    }

    async fn call(
        app: &axum::Router,
        uri: &str,
        bearer: Option<&str>,
        session: Option<&str>,
    ) -> (StatusCode, Value) {
        call_from(app, [127, 0, 0, 1], uri, bearer, session).await
    }

    async fn call_from(
        app: &axum::Router,
        source: [u8; 4],
        uri: &str,
        bearer: Option<&str>,
        session: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::HOST, "localhost");
        if let Some(bearer) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(session) = session {
            req = req.header("x-aoe-session", session);
        }
        let mut req = req.body(axum::body::Body::empty()).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                source, 5555,
            ))));
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn live_session() -> Instance {
        Instance::new("caller", "/repo")
    }

    #[tokio::test]
    async fn daemon_bearer_authenticates_with_no_caller_session() {
        let (state, _) = build_state(vec![], Some(TOKEN));
        let app = probe_router(state);
        let (status, body) = call(&app, ENDPOINT, Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"plugin": "plug", "server": "srv", "session": null})
        );
    }

    #[tokio::test]
    async fn derived_credential_authenticates_as_its_session() {
        let inst = live_session();
        let id = inst.id.clone();
        let (state, secret) = build_state(vec![inst], Some(TOKEN));
        let app = probe_router(state);
        let cred = secret.plugin_server_credential_for("plug", "srv", &id);
        let (status, body) = call(&app, ENDPOINT, Some(&cred), Some(&id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"plugin": "plug", "server": "srv", "session": id})
        );
    }

    #[tokio::test]
    async fn rejects_everything_but_the_exact_derived_tuple() {
        let inst = live_session();
        let id = inst.id.clone();
        let other = Instance::new("other", "/repo");
        let other_id = other.id.clone();
        let mut trashed = Instance::new("trashed", "/repo");
        trashed.trash();
        let trashed_id = trashed.id.clone();
        let (state, secret) = build_state(vec![inst, other, trashed], Some(TOKEN));
        let app = probe_router(state);
        let derived = |p: &str, s: &str, id: &str| secret.plugin_server_credential_for(p, s, id);

        let cases = [
            (
                "plain per-session credential",
                ENDPOINT.to_string(),
                secret.credential_for(&id),
                id.clone(),
            ),
            (
                "other server",
                "/mcp/plugins/plug/other".to_string(),
                derived("plug", "srv", &id),
                id.clone(),
            ),
            (
                "other plugin",
                "/mcp/plugins/other/srv".to_string(),
                derived("plug", "srv", &id),
                id.clone(),
            ),
            (
                "other session header",
                ENDPOINT.to_string(),
                derived("plug", "srv", &id),
                other_id,
            ),
            (
                "trashed session",
                ENDPOINT.to_string(),
                derived("plug", "srv", &trashed_id),
                trashed_id.clone(),
            ),
            (
                "mcp root",
                "/mcp".to_string(),
                derived("plug", "srv", &id),
                id.clone(),
            ),
            (
                "rest api",
                "/api/sessions".to_string(),
                derived("plug", "srv", &id),
                id.clone(),
            ),
        ];
        for (name, uri, bearer, session) in cases {
            let (status, _) = call(&app, &uri, Some(&bearer), Some(&session)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{name}");
        }
    }

    #[tokio::test]
    async fn malformed_paths_are_not_plugin_endpoints() {
        let inst = live_session();
        let id = inst.id.clone();
        let (state, secret) = build_state(vec![inst], Some(TOKEN));
        let app = probe_router(state);
        let derived = secret.plugin_server_credential_for("p", "s", &id);
        let plain = secret.credential_for(&id);
        for uri in ["/mcp/plugins/p", "/mcp/plugins/p/s/x", "/mcp/plugins//s"] {
            let (status, _) = call(&app, uri, Some(TOKEN), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri} daemon bearer");
            for bearer in [&derived, &plain] {
                let (status, _) = call(&app, uri, Some(bearer), Some(&id)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} session credential");
            }
        }
    }

    #[tokio::test]
    async fn derived_credential_is_honoured_under_auth_none() {
        let inst = live_session();
        let id = inst.id.clone();
        let (state, secret) = build_state(vec![inst], None);
        let app = probe_router(state);
        let cred = secret.plugin_server_credential_for("plug", "srv", &id);
        let (status, body) = call(&app, ENDPOINT, Some(&cred), Some(&id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["session"], json!(id));
    }

    #[tokio::test]
    async fn unix_socket_owner_is_the_daemon_unless_it_presents_the_derived_credential() {
        let inst = live_session();
        let id = inst.id.clone();
        let (state, secret) = build_state(vec![inst], Some(TOKEN));
        let app = probe_router(state);
        let cred = secret.plugin_server_credential_for("plug", "srv", &id);
        for (bearer, session) in [(None, None), (Some(cred.as_str()), Some(id.as_str()))] {
            let mut req = Request::builder()
                .method("POST")
                .uri(ENDPOINT)
                .header(header::HOST, "localhost");
            if let (Some(bearer), Some(session)) = (bearer, session) {
                req = req
                    .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                    .header("x-aoe-session", session);
            }
            let mut req = req.body(axum::body::Body::empty()).unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(
                crate::server::peer::ConnectionPeer::UnixOwner {
                    uid: nix::unistd::geteuid().as_raw(),
                },
            ));
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["session"], json!(session), "{session:?}");
        }
    }

    /// A host LAN address, as podman with pasta presents sandbox traffic.
    const LAN: [u8; 4] = [192, 168, 1, 20];

    /// Passphrase login on, with the token gate (`Some`) or passphrase-only (`None`).
    fn login_state(
        prior: Vec<Instance>,
        token: Option<&str>,
        behind_tunnel: bool,
    ) -> (Arc<AppState>, Arc<McpSessionSecret>) {
        build_state_with(prior, token, |state| {
            state.login_manager =
                Arc::new(crate::server::login::LoginManager::new(Some("passphrase")));
            state.behind_tunnel = behind_tunnel;
        })
    }

    #[tokio::test]
    async fn derived_credential_passes_the_login_wall_from_a_non_local_source() {
        for (token, behind_tunnel) in [
            (Some(TOKEN), false),
            (None, false),
            (Some(TOKEN), true),
            (None, true),
        ] {
            let inst = live_session();
            let id = inst.id.clone();
            let (state, secret) = login_state(vec![inst], token, behind_tunnel);
            let app = probe_router(state);
            let cred = secret.plugin_server_credential_for("plug", "srv", &id);
            let (status, body) = call_from(&app, LAN, ENDPOINT, Some(&cred), Some(&id)).await;
            let label = format!("token {token:?}, behind_tunnel {behind_tunnel}");
            assert_eq!(status, StatusCode::OK, "{label}: {body}");
            assert_eq!(
                body,
                json!({"plugin": "plug", "server": "srv", "session": id}),
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn login_wall_still_holds_every_other_credential_from_a_non_local_source() {
        for token in [Some(TOKEN), None] {
            let inst = live_session();
            let id = inst.id.clone();
            let other = Instance::new("other", "/repo");
            let other_id = other.id.clone();
            let mut trashed = Instance::new("trashed", "/repo");
            trashed.trash();
            let trashed_id = trashed.id.clone();
            let (state, secret) = login_state(vec![inst, other, trashed], token, false);
            let app = probe_router(state);
            let derived = secret.plugin_server_credential_for("plug", "srv", &id);
            // The token gate rejects an unverified credential before the wall; passphrase-only
            // mode has no token gate, so the wall answers.
            let invalid = if token.is_some() {
                "unauthorized"
            } else {
                "login_required"
            };
            let cases = [
                (
                    "plain session credential on /mcp",
                    "/mcp",
                    Some(secret.credential_for(&id)),
                    Some(id.clone()),
                    "login_required",
                ),
                (
                    // Passphrase-only mode configures no daemon token.
                    if token.is_some() {
                        "daemon bearer on plugin endpoint"
                    } else {
                        "unknown bearer on plugin endpoint"
                    },
                    ENDPOINT,
                    Some(TOKEN.to_string()),
                    None,
                    "login_required",
                ),
                ("no credential", ENDPOINT, None, None, invalid),
                (
                    "derived without session header",
                    ENDPOINT,
                    Some(derived.clone()),
                    None,
                    invalid,
                ),
                (
                    "derived on a third path segment",
                    "/mcp/plugins/plug/srv/x",
                    Some(derived.clone()),
                    Some(id.clone()),
                    invalid,
                ),
                (
                    "other server",
                    "/mcp/plugins/plug/other",
                    Some(derived.clone()),
                    Some(id.clone()),
                    invalid,
                ),
                (
                    "other plugin",
                    "/mcp/plugins/other/srv",
                    Some(derived.clone()),
                    Some(id.clone()),
                    invalid,
                ),
                (
                    "other session header",
                    ENDPOINT,
                    Some(derived.clone()),
                    Some(other_id.clone()),
                    invalid,
                ),
                (
                    "trashed session",
                    ENDPOINT,
                    Some(secret.plugin_server_credential_for("plug", "srv", &trashed_id)),
                    Some(trashed_id.clone()),
                    invalid,
                ),
                (
                    "derived on /mcp",
                    "/mcp",
                    Some(derived.clone()),
                    Some(id.clone()),
                    invalid,
                ),
                (
                    "derived on rest api",
                    "/api/sessions",
                    Some(derived.clone()),
                    Some(id.clone()),
                    invalid,
                ),
            ];
            for (name, uri, bearer, session, error) in cases {
                let (status, body) =
                    call_from(&app, LAN, uri, bearer.as_deref(), session.as_deref()).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{name}, token {token:?}");
                assert_eq!(body["error"], error, "{name}, token {token:?}");
            }
        }
    }

    /// A valid derived credential stamps no token hash and no principal of its own and leaves
    /// the IP's token failure budget alone. `--auth none` and `--auth passphrase` stamp a zero
    /// hash on every request by mode, and `--auth none` also the token-holder principal.
    #[tokio::test]
    async fn derived_credential_stamps_nothing_and_resets_no_failure_budget() {
        use crate::server::auth::AuthenticatedTokenHash;
        use crate::server::principal::Principal;
        use crate::server::rate_limit::AuthBudget;
        let lan = std::net::IpAddr::from(LAN);
        // (token, passphrase login, hash by mode, principal by mode)
        let cases = [
            (Some(TOKEN), false, Value::Null, Value::Null),
            (Some(TOKEN), true, Value::Null, Value::Null),
            (None, true, json!("zero"), Value::Null),
            (None, false, json!("zero"), json!("TokenHolder")),
        ];
        for (token, login, want_hash, want_principal) in cases {
            let inst = live_session();
            let id = inst.id.clone();
            let (state, secret) = build_state_with(vec![inst], token, |state| {
                if login {
                    state.login_manager =
                        Arc::new(crate::server::login::LoginManager::new(Some("passphrase")));
                }
            });
            state
                .rate_limiter
                .record_failure(lan, AuthBudget::Token)
                .await;
            let app = axum::Router::new()
                .route(
                    "/mcp/plugins/{plugin}/{server}",
                    any(
                        |hash: Option<Extension<AuthenticatedTokenHash>>,
                         principal: Option<Extension<Principal>>| async move {
                            axum::Json(json!({
                                "hash": hash.map(|h| if h.0 .0 == [0u8; 32] { "zero" } else { "token" }),
                                "principal": principal.map(|p| format!("{:?}", p.0)),
                            }))
                        },
                    ),
                )
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware,
                ))
                .with_state(state.clone());
            let cred = secret.plugin_server_credential_for("plug", "srv", &id);
            let (status, body) = call_from(&app, LAN, ENDPOINT, Some(&cred), Some(&id)).await;
            let label = format!("token {token:?}, login {login}");
            assert_eq!(status, StatusCode::OK, "{label}: {body}");
            assert_eq!(body["hash"], want_hash, "{label}");
            assert_eq!(body["principal"], want_principal, "{label}");
            assert_eq!(
                state
                    .rate_limiter
                    .failure_count(lan, AuthBudget::Token)
                    .await,
                1,
                "{label}: the plugin call must not reset the failure budget"
            );
        }
    }

    #[tokio::test]
    async fn cityhall_refuses_the_plugin_endpoint_over_tcp() {
        let inst = live_session();
        let id = inst.id.clone();
        let (state, secret) = build_state_with(vec![inst], Some(TOKEN), |state| {
            state.cityhall_mode = true;
        });
        let app = test_support::build_router_for_test(state);
        let cred = secret.plugin_server_credential_for("plug", "srv", &id);
        let (status, body) = call(&app, ENDPOINT, Some(&cred), Some(&id)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "cityhall_mode");
    }

    #[tokio::test]
    async fn full_router_mounts_the_endpoint_only_when_mcp_is_enabled() {
        let (state, _) = build_state(vec![], Some(TOKEN));
        let on = test_support::build_router_for_test(state);
        let (status, _) = call(&on, ENDPOINT, Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&on, "/mcp/plugins//srv", Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (state, _) = build_state_with(vec![], Some(TOKEN), |state| {
            state.web_config.mcp_enabled = false;
        });
        let off = test_support::build_router_for_test(state);
        let (status, _) = call(&off, ENDPOINT, Some(TOKEN), None).await;
        assert_ne!(status, StatusCode::NO_CONTENT);
    }
}
