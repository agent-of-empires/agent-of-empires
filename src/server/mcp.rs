//! Model Context Protocol server hosted on the daemon's router at `/mcp`.
//!
//! Tool handlers call the same in-process code as the HTTP handlers. Auth, host
//! and origin checks come from the router's middleware, not from `rmcp`.

use std::sync::Arc;

use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerConfig},
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::api::sessions::{list_sessions_envelope, SessionFilter};
use super::auth::AuthenticatedCallerSession;
use super::state::AppState;
use crate::daemon::{SessionResponse, SessionsEnvelope};
use crate::session::SessionScope;

#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum StateFilter {
    /// Sessions that are neither archived nor trashed.
    #[default]
    Live,
    /// Every session, including archived and trashed ones.
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListSessionsParams {
    /// Which sessions to return. Defaults to `live`.
    #[serde(default)]
    state: StateFilter,
    /// Only sessions in this group path or beneath it.
    group: Option<String>,
    /// Only direct children of the session with this id.
    children_of: Option<String>,
}

#[derive(Clone)]
struct McpServer {
    state: Arc<AppState>,
    tool_router: ToolRouter<Self>,
}

#[rmcp::tool_router]
impl McpServer {
    fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[rmcp::tool(description = "List sessions, in the same shape as `GET /api/sessions`.")]
    async fn list_sessions(
        &self,
        Parameters(params): Parameters<ListSessionsParams>,
    ) -> Json<SessionsEnvelope> {
        let filter = SessionFilter {
            scope: Some(match params.state {
                StateFilter::Live => SessionScope::Live,
                StateFilter::All => SessionScope::All,
            }),
            group: params.group,
            children_of: params.children_of,
        };
        // A read-only tool that agents may poll must not persist the dashboard's ordering.
        Json(list_sessions_envelope(&self.state, &filter, false).await)
    }

    #[rmcp::tool(
        description = "The calling session, or null when the caller is not a session AoE launched."
    )]
    async fn whoami(&self, context: RequestContext<RoleServer>) -> Json<Option<SessionResponse>> {
        let caller_session_id = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<AuthenticatedCallerSession>())
            .map(|AuthenticatedCallerSession(id)| id.clone());
        let Some(id) = caller_session_id else {
            return Json(None);
        };
        let instances = self.state.instances.read().await;
        let response = instances.iter().find(|inst| inst.id == id).map(|inst| {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        });
        Json(response)
    }
}

#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("aoe", env!("CARGO_PKG_VERSION")))
            .with_instructions("Read Agent of Empires sessions.")
    }
}

pub(super) fn service(
    state: Arc<AppState>,
) -> StreamableHttpService<impl ServerHandler, NeverSessionManager> {
    StreamableHttpService::new(
        move || Ok(McpServer::new(state.clone())),
        Arc::default(),
        // `rmcp` reads the body itself, so the router's `DefaultBodyLimit` does not apply.
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts()
            .with_max_request_body_bytes(1024 * 1024),
    )
}

#[cfg(test)]
mod tests {
    use axum::http::{header, Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::server::test_helpers::vecs;
    use crate::server::test_support;
    use crate::session::Instance;

    const TOKEN: &str = "mcp-test-token";

    fn app(mcp_enabled: bool, prior: Vec<Instance>) -> axum::Router {
        let mut state = test_support::build_test_app_state_with_policy(
            prior,
            vecs(&["localhost"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        std::sync::Arc::get_mut(&mut state)
            .expect("unique test state")
            .web_config
            .mcp_enabled = mcp_enabled;
        test_support::build_router_for_test(state)
    }

    /// Builds app state with `mcp_enabled` and a session-credential secret, under the given
    /// daemon token policy (`None` means `--auth=none`).
    fn mcp_session_secret_state(
        prior: Vec<Instance>,
        token: Option<&str>,
    ) -> (
        std::sync::Arc<super::AppState>,
        std::sync::Arc<crate::server::session_credential::McpSessionSecret>,
    ) {
        let mut state = test_support::build_test_app_state_with_policy(
            prior,
            vecs(&["localhost"]),
            Vec::new(),
            token.map(str::to_string),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = std::sync::Arc::new(
            crate::server::session_credential::McpSessionSecret::load_or_generate(
                &dir.path().join("mcp_session_secret"),
            )
            .expect("load_or_generate"),
        );
        let state_mut = std::sync::Arc::get_mut(&mut state).expect("unique test state");
        state_mut.web_config.mcp_enabled = true;
        state_mut.mcp_session_secret = Some(secret.clone());
        (state, secret)
    }

    /// Like [`app`] with `mcp_enabled`, plus a session-credential secret so a caller can
    /// authenticate as a specific session via `X-AoE-Session` + its HMAC bearer.
    fn app_with_mcp_session_secret(
        prior: Vec<Instance>,
    ) -> (
        axum::Router,
        std::sync::Arc<crate::server::session_credential::McpSessionSecret>,
    ) {
        let (state, secret) = mcp_session_secret_state(prior, Some(TOKEN));
        (test_support::build_router_for_test(state), secret)
    }

    /// Like [`app_with_mcp_session_secret`], but with the daemon token gate disabled
    /// (`--auth=none`).
    fn app_with_mcp_session_secret_no_auth(
        prior: Vec<Instance>,
    ) -> (
        axum::Router,
        std::sync::Arc<crate::server::session_credential::McpSessionSecret>,
    ) {
        let (state, secret) = mcp_session_secret_state(prior, None);
        (test_support::build_router_for_test(state), secret)
    }

    async fn send(
        app: &axum::Router,
        mut req: Request<axum::body::Body>,
    ) -> axum::response::Response {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                5555,
            ))));
        app.clone().oneshot(req).await.unwrap()
    }

    /// POST one JSON-RPC message to `/mcp`, with arbitrary extra headers beyond the bearer
    /// (e.g. `X-AoE-Session`); returns the status and the JSON-RPC response.
    async fn rpc_with_headers(
        app: &axum::Router,
        token: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: Value,
    ) -> (StatusCode, Option<Value>) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in extra_headers {
            req = req.header(*name, *value);
        }
        let resp = send(
            app,
            req.body(axum::body::Body::from(body.to_string())).unwrap(),
        )
        .await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).ok())
    }

    /// POST one JSON-RPC message to `/mcp`; returns the status and the JSON-RPC response.
    async fn rpc(
        app: &axum::Router,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Option<Value>) {
        rpc_with_headers(app, token, &[], body).await
    }

    async fn initialize(app: &axum::Router) {
        let (status, message) = rpc(
            app,
            Some(TOKEN),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1"}
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let result = message.unwrap()["result"].clone();
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["serverInfo"]["name"], "aoe");
        let (status, _) = rpc(
            app,
            Some(TOKEN),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    async fn call(app: &axum::Router, name: &str, arguments: Value) -> Value {
        let (status, message) = rpc(
            app,
            Some(TOKEN),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let result = message.unwrap()["result"].clone();
        assert_ne!(result["isError"], json!(true), "{result}");
        result["structuredContent"].clone()
    }

    fn session_in(title: &str, group: &str, parent: Option<&str>) -> Instance {
        let mut inst = Instance::new(title, "/repo");
        inst.group_path = group.to_string();
        inst.parent_session_id = parent.map(str::to_string);
        inst
    }

    #[tokio::test]
    async fn route_exists_only_when_setting_is_on() {
        let off = app(false, vec![]);
        let (status, _) = rpc(&off, Some(TOKEN), json!({"jsonrpc": "2.0"})).await;
        let unrouted = Request::builder()
            .method("POST")
            .uri("/nope")
            .header(header::HOST, "localhost")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let nope = send(&off, unrouted).await.status();
        assert_eq!(status, nope, "off behaves like any unrouted path");
        // The dashboard build answers unknown paths from a GET-only SPA fallback.
        let expected = if cfg!(feature = "web") {
            StatusCode::METHOD_NOT_ALLOWED
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(status, expected);

        initialize(&app(true, vec![])).await;
    }

    /// With passphrase login on, a remote caller without a login session gets a JSON `401`, not a
    /// redirect to the login page, whether or not it presents a valid token.
    #[tokio::test]
    async fn login_wall_answers_mcp_with_401_not_a_redirect() {
        for token in [None, Some(TOKEN)] {
            let mut state = test_support::build_test_app_state_with_policy(
                Vec::new(),
                vecs(&["localhost"]),
                Vec::new(),
                token.map(str::to_string),
            );
            let state_mut = std::sync::Arc::get_mut(&mut state).expect("unique test state");
            state_mut.web_config.mcp_enabled = true;
            state_mut.login_manager =
                std::sync::Arc::new(crate::server::login::LoginManager::new(Some("passphrase")));
            let app = test_support::build_router_for_test(state);

            let mut req = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream");
            if let Some(token) = token {
                req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let mut req = req
                .body(axum::body::Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [203, 0, 113, 7],
                    5555,
                ))));
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "token {token:?}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], "login_required", "token {token:?}");
        }
    }

    #[tokio::test]
    async fn requests_without_a_valid_token_are_rejected() {
        let app = app(true, vec![]);
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        for token in [None, Some("wrong-token")] {
            let (status, _) = rpc(&app, token, body.clone()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
        }
    }

    #[tokio::test]
    async fn tools_list_advertises_both_tools() {
        let app = app(true, vec![]);
        // No `initialize` first: the server keeps no session state between requests.
        let (status, message) = rpc(
            &app,
            Some(TOKEN),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tools = message.unwrap()["result"]["tools"].clone();
        let mut names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["list_sessions", "whoami"]);
        for tool in tools.as_array().unwrap() {
            assert!(tool["outputSchema"].is_object(), "{tool}");
        }
        let list = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "list_sessions")
            .unwrap();
        let mut params: Vec<&str> = list["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        params.sort_unstable();
        assert_eq!(params, ["children_of", "group", "state"]);
    }

    #[tokio::test]
    async fn whoami_is_null() {
        let app = app(true, vec![]);
        assert_eq!(call(&app, "whoami", json!({})).await, Value::Null);
    }

    /// The daemon-wide bearer still authenticates `/mcp` with no `X-AoE-Session` header, even
    /// once a session-credential secret exists; `whoami` has no caller identity to report.
    #[tokio::test]
    async fn whoami_is_null_for_the_plain_daemon_bearer_even_with_a_secret_configured() {
        let (app, _secret) = app_with_mcp_session_secret(vec![]);
        assert_eq!(call(&app, "whoami", json!({})).await, Value::Null);
    }

    /// `--auth=none` skips the token gate entirely, but a caller presenting a valid session
    /// credential should still resolve its own identity.
    #[tokio::test]
    async fn whoami_resolves_the_calling_session_under_auth_none() {
        let inst = session_in("caller", "work", None);
        let id = inst.id.clone();
        let (app, secret) = app_with_mcp_session_secret_no_auth(vec![inst]);
        let bearer = secret.credential_for(&id);

        let (status, message) = rpc_with_headers(
            &app,
            Some(&bearer),
            &[("x-aoe-session", &id)],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let result = message.unwrap()["result"].clone();
        assert_ne!(result["isError"], json!(true), "{result}");
        assert_eq!(result["structuredContent"]["id"], id);
    }

    /// `--auth=passphrase` bypasses the passphrase wall entirely for a loopback caller (the
    /// common case for an agent calling its own daemon); that bypass must not also suppress
    /// caller-session identity for a request that otherwise carries a valid credential.
    #[tokio::test]
    async fn whoami_resolves_the_calling_session_under_auth_passphrase_on_loopback() {
        let inst = session_in("caller", "work", None);
        let id = inst.id.clone();
        let (mut state, secret) = mcp_session_secret_state(vec![inst], None);
        std::sync::Arc::get_mut(&mut state)
            .expect("unique test state")
            .login_manager =
            std::sync::Arc::new(crate::server::login::LoginManager::new(Some("passphrase")));
        let app = test_support::build_router_for_test(state);
        let bearer = secret.credential_for(&id);

        let (status, message) = rpc_with_headers(
            &app,
            Some(&bearer),
            &[("x-aoe-session", &id)],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let result = message.unwrap()["result"].clone();
        assert_ne!(result["isError"], json!(true), "{result}");
        assert_eq!(result["structuredContent"]["id"], id);
    }

    /// Off-box, a valid session credential must not bypass the passphrase wall: that wall
    /// exists precisely to stop a leaked credential from being used remotely.
    #[tokio::test]
    async fn session_credential_does_not_bypass_the_passphrase_wall_for_a_remote_caller() {
        let inst = session_in("caller", "work", None);
        let id = inst.id.clone();
        let (mut state, secret) = mcp_session_secret_state(vec![inst], None);
        std::sync::Arc::get_mut(&mut state)
            .expect("unique test state")
            .login_manager =
            std::sync::Arc::new(crate::server::login::LoginManager::new(Some("passphrase")));
        let app = test_support::build_router_for_test(state);
        let bearer = secret.credential_for(&id);

        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header("x-aoe-session", &id)
            .body(axum::body::Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                5555,
            ))));
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "login_required");
    }

    /// A browser-authenticated request (valid device-bound login session) that also happens to
    /// carry a valid `/mcp` session credential must still resolve caller identity: the two are
    /// orthogonal, and this is the one dispatch path that grants access without ever reaching
    /// `attach_caller_session`.
    #[tokio::test]
    async fn whoami_resolves_the_calling_session_for_a_browser_authenticated_request() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let inst = session_in("caller", "work", None);
        let id = inst.id.clone();
        let (mut state, secret) = mcp_session_secret_state(vec![inst], Some(TOKEN));
        let login =
            std::sync::Arc::new(crate::server::login::LoginManager::new(Some("passphrase")));
        let binding = [0xAB; 32];
        let browser_session_id = login
            .create_session(&binding, "203.0.113.7", "test-agent")
            .await;
        std::sync::Arc::get_mut(&mut state)
            .expect("unique test state")
            .login_manager = login;
        let app = test_support::build_router_for_test(state);
        let bearer = secret.credential_for(&id);

        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header("cookie", format!("aoe_session={browser_session_id}"))
            .header("x-aoe-device-binding", URL_SAFE_NO_PAD.encode(binding))
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header("x-aoe-session", &id)
            .body(axum::body::Body::from(
                tools_call(1, "whoami", json!({})).to_string(),
            ))
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                5555,
            ))));
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let message: Value = serde_json::from_slice(&bytes).unwrap();
        let result = message["result"].clone();
        assert_ne!(result["isError"], json!(true), "{result}");
        assert_eq!(result["structuredContent"]["id"], id);
    }

    fn tools_call(id: i64, name: &str, arguments: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
    }

    #[tokio::test]
    async fn whoami_resolves_the_calling_session_for_a_valid_credential() {
        let inst = session_in("caller", "work", None);
        let id = inst.id.clone();
        let (app, secret) = app_with_mcp_session_secret(vec![inst]);
        let bearer = secret.credential_for(&id);

        let (status, message) = rpc_with_headers(
            &app,
            Some(&bearer),
            &[("x-aoe-session", &id)],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let result = message.unwrap()["result"].clone();
        assert_ne!(result["isError"], json!(true), "{result}");
        assert_eq!(result["structuredContent"]["id"], id);
    }

    #[tokio::test]
    async fn session_credential_is_rejected_when_the_header_names_a_different_session() {
        let a = session_in("a", "work", None);
        let b = session_in("b", "work", None);
        let (a_id, b_id) = (a.id.clone(), b.id.clone());
        let (app, secret) = app_with_mcp_session_secret(vec![a, b]);
        // Bearer is valid for `a`, but the header claims to be `b`.
        let bearer_for_a = secret.credential_for(&a_id);

        let (status, _) = rpc_with_headers(
            &app,
            Some(&bearer_for_a),
            &[("x-aoe-session", &b_id)],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_credential_is_rejected_for_an_unknown_session() {
        let (app, secret) = app_with_mcp_session_secret(vec![]);
        let bearer = secret.credential_for("no-such-session");

        let (status, _) = rpc_with_headers(
            &app,
            Some(&bearer),
            &[("x-aoe-session", "no-such-session")],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_credential_is_rejected_for_a_trashed_session() {
        let mut inst = session_in("trashed", "work", None);
        inst.trash();
        let id = inst.id.clone();
        let (app, secret) = app_with_mcp_session_secret(vec![inst]);
        let bearer = secret.credential_for(&id);

        let (status, _) = rpc_with_headers(
            &app,
            Some(&bearer),
            &[("x-aoe-session", &id)],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_credential_is_rejected_when_the_bearer_does_not_match_the_hmac() {
        let inst = session_in("caller", "work", None);
        let id = inst.id.clone();
        let (app, _secret) = app_with_mcp_session_secret(vec![inst]);

        let (status, _) = rpc_with_headers(
            &app,
            Some("not-the-real-hmac"),
            &[("x-aoe-session", &id)],
            tools_call(1, "whoami", json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn list_sessions_honours_state_group_and_children_of() {
        // The writable listing path persists workspace ordering into the app dir.
        let _guard = crate::session::test_support::isolate_app_dir();
        let parent = session_in("parent", "work", None);
        let child = session_in("child", "work/sub", Some(&parent.id));
        let other = session_in("other", "elsewhere", None);
        let lookalike = session_in("lookalike", "workshop", None);
        let mut trashed = session_in("trashed", "work", None);
        trashed.trash();
        let (parent_id, child_id) = (parent.id.clone(), child.id.clone());
        let app = app(true, vec![parent, child, other, lookalike, trashed]);

        let titles = |envelope: Value| -> Vec<String> {
            let mut titles: Vec<String> = envelope["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["title"].as_str().unwrap().to_string())
                .collect();
            titles.sort();
            titles
        };

        // (arguments, expected titles)
        let cases = [
            (json!({}), vec!["child", "lookalike", "other", "parent"]),
            (
                json!({"state": "all"}),
                vec!["child", "lookalike", "other", "parent", "trashed"],
            ),
            (json!({"group": "work"}), vec!["child", "parent"]),
            (json!({"group": "work/sub"}), vec!["child"]),
            (
                json!({"group": "work", "state": "all"}),
                vec!["child", "parent", "trashed"],
            ),
            (json!({"children_of": parent_id}), vec!["child"]),
            (json!({"children_of": child_id}), vec![]),
        ];
        for (arguments, expected) in cases {
            let envelope = call(&app, "list_sessions", arguments.clone()).await;
            assert_eq!(titles(envelope), expected, "{arguments}");
        }
        assert!(
            crate::session::load_workspace_ordering()
                .unwrap()
                .order
                .is_empty(),
            "listing through MCP must not persist the dashboard's workspace ordering"
        );
    }
}
