//! Daemon endpoints `/mcp/plugins/{plugin}/{server}` behind which plugins serve MCP servers.

use std::sync::Arc;
use std::time::Duration;

use aoe_plugin_api::{is_valid_mcp_server_name, PluginManifest, MCP_SERVE_CAPABILITY};
use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ResultType, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    session::never::NeverSessionManager, StreamableHttpService,
};
use rmcp::{ErrorData, ServerHandler};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::auth::AuthenticatedPluginCaller;
use super::state::AppState;
use crate::plugin::worker_call::WorkerCall;
use crate::plugin::worker_requests::WorkerRequestError;

/// The route service for `/mcp/plugins/{plugin}/{server}`: the MCP transport answers every
/// method itself.
pub(super) fn service(state: Arc<AppState>) -> axum::routing::MethodRouter {
    axum::routing::any(plugin_endpoint).with_state(state)
}

/// Ceiling for one forwarded request. The daemon keeps no per-server state, so the worker
/// enforces any tighter limit itself.
pub const MCP_SERVE_CALL_TIMEOUT: Duration = Duration::from_millis(600_000);
/// How long a request waits for a worker that is not running yet, so a plugin whose worker
/// is gone fails fast instead of holding the call for the whole ceiling.
pub const MCP_SERVE_START_WAIT: Duration = Duration::from_secs(5);
const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
const CALL_FAILED_TEXT: &str = "The plugin MCP server could not complete this call.";
const LIST_FAILED_TEXT: &str = "The plugin MCP server is unavailable.";

/// Serves `/mcp/plugins/{plugin}/{server}`. Plugin and server come from the authenticated
/// caller, which auth parsed from the raw path, never from a percent-decoded `Path`.
pub(super) async fn plugin_endpoint(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<AuthenticatedPluginCaller>>,
    request: Request,
) -> Response {
    let Some(Extension(caller)) = caller else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let servable = is_valid_mcp_server_name(&caller.server) && {
        let registry = crate::plugin::registry();
        serves_mcp(registry.active().map(|p| &p.manifest), &caller.plugin)
    };
    if !servable {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(host) = state.plugin_host.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    forward(host, caller, request).await
}

/// Whether `plugin` is among `manifests` and declares `mcp.serve`.
fn serves_mcp<'a>(mut manifests: impl Iterator<Item = &'a PluginManifest>, plugin: &str) -> bool {
    manifests.any(|m| {
        m.id.as_str() == plugin
            && m.capabilities
                .iter()
                .any(|c| c.as_str() == MCP_SERVE_CAPABILITY)
    })
}

async fn forward(
    worker: Arc<dyn WorkerCall>,
    caller: AuthenticatedPluginCaller,
    request: Request,
) -> Response {
    let handler = PluginServerHandler {
        worker,
        caller: Arc::new(caller),
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        super::mcp::stateless_config(),
    );
    service.handle(request).await.map(axum::body::Body::new)
}

/// A result the handler parses from worker JSON.
trait WorkerResult: DeserializeOwned {
    fn result_type(&mut self) -> &mut Option<ResultType>;
}

impl WorkerResult for ListToolsResult {
    fn result_type(&mut self) -> &mut Option<ResultType> {
        &mut self.result_type
    }
}

impl WorkerResult for CallToolResult {
    fn result_type(&mut self) -> &mut Option<ResultType> {
        &mut self.result_type
    }
}

#[derive(Clone)]
struct PluginServerHandler {
    worker: Arc<dyn WorkerCall>,
    caller: Arc<AuthenticatedPluginCaller>,
}

/// Why a forwarded request failed. Only the category is logged: the worker's own error text
/// never reaches aoe logs or the client.
enum Failure {
    Worker(WorkerRequestError),
    TooLarge,
    Malformed,
    Cancelled,
}

impl Failure {
    fn category(&self) -> &'static str {
        match self {
            Self::Worker(WorkerRequestError::NoWorker) => "no_worker",
            Self::Worker(WorkerRequestError::Timeout) => "timeout",
            Self::Worker(WorkerRequestError::WorkerGone) => "worker_gone",
            Self::Worker(WorkerRequestError::Rejected) => "rejected",
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl PluginServerHandler {
    fn params(&self, extra: Value) -> Value {
        let mut params = json!({
            "server": self.caller.server,
            "session_id": self.caller.session,
        });
        if let (Some(params), Value::Object(extra)) = (params.as_object_mut(), extra) {
            params.extend(extra);
        }
        params
    }

    async fn request<T: WorkerResult>(
        &self,
        method: &'static str,
        params: Value,
        context: &RequestContext<RoleServer>,
    ) -> Result<T, Failure> {
        let result = self.exchange(method, params, context).await;
        let (plugin, server) = (&self.caller.plugin, &self.caller.server);
        match &result {
            Ok(_) => {}
            Err(Failure::Cancelled) => {
                tracing::debug!(%plugin, %server, method, "plugin MCP request cancelled");
            }
            Err(failure) => tracing::warn!(
                %plugin,
                %server,
                method,
                failure = failure.category(),
                "plugin MCP request failed"
            ),
        }
        result
    }

    async fn exchange<T: WorkerResult>(
        &self,
        method: &'static str,
        params: Value,
        context: &RequestContext<RoleServer>,
    ) -> Result<T, Failure> {
        let plugin = self.caller.plugin.as_str();
        // The envelope id the worker sees, so `mcp.cancel` names this request unambiguously.
        let id = self.worker.request_id();
        let call = self.worker.call_as(
            &id,
            plugin,
            method,
            params,
            MCP_SERVE_START_WAIT,
            MCP_SERVE_CALL_TIMEOUT,
        );
        let reply = tokio::select! {
            reply = call => reply,
            // rmcp cancels the context when the HTTP client goes away mid-request.
            () = context.ct.cancelled() => {
                self.cancel(&id).await;
                return Err(Failure::Cancelled);
            }
        };
        if matches!(reply, Err(WorkerRequestError::Timeout)) {
            self.cancel(&id).await;
        }
        let value = reply.map_err(Failure::Worker)?;
        if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > MAX_RESULT_BYTES) {
            return Err(Failure::TooLarge);
        }
        let mut result: T = serde_json::from_value(value).map_err(|_| Failure::Malformed)?;
        // 2026-07-28 clients require `resultType`; rmcp strips `complete` for older peers. The
        // handler only serves complete results, so any other worker value is malformed.
        let result_type = result.result_type();
        if result_type.as_ref().is_some_and(|t| !t.is_complete()) {
            return Err(Failure::Malformed);
        }
        *result_type = Some(ResultType::COMPLETE);
        Ok(result)
    }

    async fn cancel(&self, id: &str) {
        let params = self.params(json!({"request_id": id}));
        self.worker
            .notify(&self.caller.plugin, "mcp.cancel", params)
            .await;
    }
}

impl ServerHandler for PluginServerHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(self.caller.server.clone(), env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let params = match request.and_then(|r| r.cursor) {
            Some(cursor) => self.params(json!({"cursor": cursor})),
            None => self.params(json!({})),
        };
        self.request("mcp.tools.list", params, &context)
            .await
            .map_err(|_| ErrorData::internal_error(LIST_FAILED_TEXT, None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let params = self.params(json!({
            "name": request.name,
            "arguments": request.arguments.unwrap_or_default(),
        }));
        let result = self
            .request::<CallToolResult>("mcp.tools.call", params, &context)
            .await
            .unwrap_or_else(|_| CallToolResult::error(vec![ContentBlock::text(CALL_FAILED_TEXT)]));
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::Extension;
    use axum::http::{header, Request, StatusCode};
    use axum::routing::any;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    use super::*;
    use crate::plugin::worker_requests::WorkerRequestError;
    use crate::server::auth::auth_middleware;
    use crate::server::session_credential::McpSessionSecret;
    use crate::server::test_helpers::vecs;
    use crate::server::test_support;
    use crate::session::Instance;

    const TOKEN: &str = "plugin-endpoint-test-token";
    const ENDPOINT: &str = "/mcp/plugins/plug/srv";
    const DEADLINE: Duration = Duration::from_secs(10);

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

    fn manifest(id: &str, caps: &str) -> PluginManifest {
        PluginManifest::from_toml_str(&format!(
            "id = \"{id}\"\nname = \"N\"\nversion = \"1.0.0\"\napi_version = 22\ncapabilities = [{caps}]\n\n[runtime]\nkind = \"command\"\ncommand = [\"bin/w\"]\n"
        ))
        .unwrap()
    }

    #[test]
    fn serves_mcp_needs_the_plugin_with_the_capability() {
        let ms = [
            manifest("acme.serve", "\"runtime.worker\", \"mcp.serve\""),
            manifest("acme.plain", "\"runtime.worker\""),
        ];
        let cases = [
            ("acme.serve", true),
            ("acme.plain", false),
            ("acme.absent", false),
        ];
        for (plugin, want) in cases {
            assert_eq!(serves_mcp(ms.iter(), plugin), want, "{plugin}");
        }
    }

    /// Gating runs against the live registry, so the env and registry are isolated.
    #[tokio::test]
    #[serial_test::serial]
    async fn full_router_gates_on_an_active_mcp_serve_plugin_and_a_valid_name() {
        let temp = tempfile::tempdir().unwrap();
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("XDG_CONFIG_HOME", temp.path().to_path_buf()),
            ("HOME", temp.path().to_path_buf()),
            ("USERPROFILE", temp.path().to_path_buf()),
        ]);
        let worker = r#"["sleep", "600"]"#;
        crate::plugin::install_test_plugin(
            "acme.serve",
            22,
            &["runtime.worker", "mcp.serve"],
            worker,
        );
        crate::plugin::install_test_plugin("acme.plain", 20, &["runtime.worker"], worker);
        crate::plugin::reload_registry();

        let (state, _) = build_state(vec![], Some(TOKEN));
        let on = test_support::build_router_for_test(state);
        let cases = [
            ("/mcp/plugins//srv", StatusCode::NOT_FOUND),
            ("/mcp/plugins/acme.absent/srv", StatusCode::NOT_FOUND),
            ("/mcp/plugins/acme.plain/srv", StatusCode::NOT_FOUND),
            ("/mcp/plugins/acme.serve/-srv", StatusCode::NOT_FOUND),
            ("/mcp/plugins/acme.serve/a%2Fb", StatusCode::NOT_FOUND),
            // Gating passed; this test state runs no plugin host.
            (
                "/mcp/plugins/acme.serve/srv",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ];
        for (uri, want) in cases {
            let (status, _) = call(&on, uri, Some(TOKEN), None).await;
            assert_eq!(status, want, "{uri}");
        }

        let (state, _) = build_state_with(vec![], Some(TOKEN), |state| {
            state.web_config.mcp_enabled = false;
        });
        let off = test_support::build_router_for_test(state);
        let (status, _) = call(&off, "/mcp/plugins/acme.serve/srv", Some(TOKEN), None).await;
        assert_ne!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Calls are `(envelope id, method, params)`, notifications `(method, params)`.
    type Call = (String, String, Value);
    type Note = (String, Value);

    /// `reply` answers every call; `None` never resolves.
    struct FakeWorker {
        reply: Box<dyn Fn() -> Option<Result<Value, WorkerRequestError>> + Send + Sync>,
        next_id: std::sync::atomic::AtomicU64,
        calls: mpsc::UnboundedSender<Call>,
        notes: mpsc::UnboundedSender<Note>,
    }

    impl WorkerCall for FakeWorker {
        fn call<'a>(
            &'a self,
            _plugin_id: &'a str,
            _method: &'a str,
            _params: Value,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
            unreachable!("forwarding sends with a correlatable id")
        }

        fn request_id(&self) -> String {
            let n = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("h-fake-{n}")
        }

        fn call_as<'a>(
            &'a self,
            id: &'a str,
            plugin_id: &'a str,
            method: &'a str,
            params: Value,
            start_wait: Duration,
            timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, WorkerRequestError>> {
            assert_eq!(plugin_id, "acme.plug");
            assert_eq!(start_wait, MCP_SERVE_START_WAIT);
            assert_eq!(timeout, MCP_SERVE_CALL_TIMEOUT);
            let _ = self
                .calls
                .send((id.to_string(), method.to_string(), params));
            match (self.reply)() {
                Some(reply) => Box::pin(async move { reply }),
                None => Box::pin(std::future::pending()),
            }
        }

        fn notify<'a>(
            &'a self,
            plugin_id: &'a str,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, bool> {
            assert_eq!(plugin_id, "acme.plug");
            let _ = self.notes.send((method.to_string(), params));
            Box::pin(async { true })
        }

        fn report_failure(&self, _plugin_id: &str, _message: &str) {
            unreachable!("plugin MCP forwarding reports no host failures")
        }
    }

    struct Fake {
        app: axum::Router,
        calls: mpsc::UnboundedReceiver<Call>,
        notes: mpsc::UnboundedReceiver<Note>,
    }

    /// `forward` behind a router that stamps the caller the way auth does.
    fn fake(
        session: Option<&str>,
        reply: impl Fn() -> Option<Result<Value, WorkerRequestError>> + Send + Sync + 'static,
    ) -> Fake {
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let (notes_tx, notes) = mpsc::unbounded_channel();
        let worker: Arc<dyn WorkerCall> = Arc::new(FakeWorker {
            reply: Box::new(reply),
            next_id: Default::default(),
            calls: calls_tx,
            notes: notes_tx,
        });
        let caller = AuthenticatedPluginCaller {
            plugin: "acme.plug".into(),
            server: "srv".into(),
            session: session.map(str::to_string),
        };
        let app = axum::Router::new().route(
            "/mcp/plugins/{plugin}/{server}",
            any(move |req: axum::extract::Request| forward(worker.clone(), caller.clone(), req)),
        );
        Fake { app, calls, notes }
    }

    async fn rpc(app: &axum::Router, body: Value) -> (StatusCode, Value) {
        post(app, &[], body).await
    }

    /// The reply is parsed as plain JSON, never into rmcp types, so a missing wire field shows.
    async fn post(
        app: &axum::Router,
        headers: &[(&str, String)],
        body: Value,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp/plugins/acme.plug/srv")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        for (name, value) in headers {
            req = req.header(*name, value);
        }
        let req = req.body(axum::body::Body::from(body.to_string())).unwrap();
        // A panicking handler never answers; fail instead of hanging.
        let resp = tokio::time::timeout(DEADLINE, app.clone().oneshot(req))
            .await
            .expect("the endpoint answers")
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn list_request() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
    }

    fn list_page_request(cursor: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"cursor": cursor}})
    }

    fn call_request(id: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "echo", "arguments": {"x": 1}}
        })
    }

    /// How a client of each protocol generation sends a request.
    #[derive(Clone, Copy, Debug)]
    enum Client {
        /// No version header.
        Unversioned,
        /// A 2025 client after `initialize`: the negotiated version as a header only.
        Legacy,
        /// A stateless 2026-07-28 client: per-request `_meta` plus the method headers.
        Modern,
    }

    impl Client {
        const ALL: [Self; 3] = [Self::Unversioned, Self::Legacy, Self::Modern];

        fn shape(self, mut body: Value) -> (Vec<(&'static str, String)>, Value) {
            let version = match self {
                Self::Unversioned => return (Vec::new(), body),
                Self::Legacy => {
                    return (vec![("mcp-protocol-version", "2025-06-18".into())], body);
                }
                Self::Modern => "2026-07-28",
            };
            let mut headers = vec![
                ("mcp-protocol-version", version.to_string()),
                ("mcp-method", body["method"].as_str().unwrap().to_string()),
            ];
            if let Some(name) = body["params"]["name"].as_str() {
                headers.push(("mcp-name", name.to_string()));
            }
            body["params"]["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": version,
                "io.modelcontextprotocol/clientCapabilities": {},
            });
            (headers, body)
        }

        /// The wire `resultType` of a result: modern clients reject one without it, and rmcp
        /// strips it for older peers.
        fn result_type(self) -> Value {
            match self {
                Self::Modern => json!("complete"),
                Self::Unversioned | Self::Legacy => Value::Null,
            }
        }
    }

    #[tokio::test]
    async fn results_built_from_worker_json_carry_result_type_for_modern_clients() {
        let tool = json!({"name": "echo", "inputSchema": {"type": "object"}});
        type Reply = Arc<dyn Fn() -> Result<Value, WorkerRequestError> + Send + Sync>;
        let ok = |v: Value| -> Reply { Arc::new(move || Ok(v.clone())) };
        // (case, worker reply, request, answered with a result rather than a JSON-RPC error)
        let cases: Vec<(&str, Reply, Value, bool)> = vec![
            (
                "tools/list",
                ok(json!({"tools": [tool]})),
                list_request(),
                true,
            ),
            (
                "tools/list already complete",
                ok(json!({"tools": [tool], "resultType": "complete"})),
                list_request(),
                true,
            ),
            (
                "tools/list not complete",
                ok(json!({"tools": [tool], "resultType": "input_required"})),
                list_request(),
                false,
            ),
            (
                "tools/call",
                ok(json!({"content": [{"type": "text", "text": "hi"}]})),
                call_request(json!(2)),
                true,
            ),
            (
                "tools/call tool error",
                ok(json!({"content": [{"type": "text", "text": "no"}], "isError": true})),
                call_request(json!(2)),
                true,
            ),
            (
                "tools/call not complete",
                ok(json!({"content": [], "resultType": "task"})),
                call_request(json!(2)),
                true,
            ),
            (
                "failed tools/call",
                Arc::new(|| Err(WorkerRequestError::NoWorker)),
                call_request(json!(2)),
                true,
            ),
        ];
        for (case, reply, request, answered) in cases {
            let fake = fake(None, move || Some(reply()));
            for client in Client::ALL {
                let (headers, body) = client.shape(request.clone());
                let (status, reply) = post(&fake.app, &headers, body).await;
                let label = format!("{client:?} {case}: {reply}");
                assert_eq!(status, StatusCode::OK, "{label}");
                if !answered {
                    assert!(reply["result"].is_null(), "{label}");
                    assert!(reply["error"]["code"].is_i64(), "{label}");
                    continue;
                }
                let result = &reply["result"];
                assert!(result.is_object(), "{label}");
                assert_eq!(result["resultType"], client.result_type(), "{label}");
            }
        }
    }

    fn drain<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> Vec<T> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// The forwarded `(method, params)`, checking each carried a fresh host id.
    fn forwarded(rx: &mut mpsc::UnboundedReceiver<Call>) -> Vec<(String, Value)> {
        drain(rx)
            .into_iter()
            .map(|(id, method, params)| {
                assert!(id.starts_with("h-fake-"), "{id}");
                (method, params)
            })
            .collect()
    }

    #[tokio::test]
    async fn initialize_and_tools_list_return_the_workers_tools() {
        let tools = json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]});
        let reply = tools.clone();
        let mut fake = fake(None, move || Some(Ok(reply.clone())));

        let (status, init) = rpc(
            &fake.app,
            json!({
                "jsonrpc": "2.0",
                "id": 0,
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
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );
        assert!(drain(&mut fake.calls).is_empty(), "initialize stays local");

        let cases = [
            (list_request(), json!({"server": "srv", "session_id": null})),
            (
                list_page_request("page-2"),
                json!({"server": "srv", "session_id": null, "cursor": "page-2"}),
            ),
        ];
        for (request, params) in cases {
            let (status, list) = rpc(&fake.app, request).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
            assert_eq!(
                forwarded(&mut fake.calls),
                [("mcp.tools.list".to_string(), params)]
            );
        }
        assert!(drain(&mut fake.notes).is_empty());
    }

    #[tokio::test]
    async fn tools_call_forwards_name_arguments_server_and_session() {
        for session in [None, Some("sess-1")] {
            let mut fake = fake(session, || {
                Some(Ok(json!({"content": [{"type": "text", "text": "hi"}]})))
            });
            let (status, body) = rpc(&fake.app, call_request(json!(2))).await;
            assert_eq!(status, StatusCode::OK);
            let result = &body["result"];
            assert_ne!(result["isError"], json!(true), "{body}");
            assert_eq!(result["content"][0]["text"], "hi", "{body}");
            assert_eq!(
                forwarded(&mut fake.calls),
                [(
                    "mcp.tools.call".to_string(),
                    json!({
                        "server": "srv",
                        "session_id": session,
                        "name": "echo",
                        "arguments": {"x": 1},
                    }),
                )],
                "{session:?}"
            );
            assert!(drain(&mut fake.notes).is_empty());
        }
    }

    #[tokio::test]
    async fn worker_failures_map_to_generic_errors() {
        let huge = "x".repeat(MAX_RESULT_BYTES);
        type Reply = Box<dyn Fn() -> Result<Value, WorkerRequestError> + Send + Sync>;
        let cases: Vec<(&str, Reply)> = vec![
            ("no worker", Box::new(|| Err(WorkerRequestError::NoWorker))),
            ("timeout", Box::new(|| Err(WorkerRequestError::Timeout))),
            ("rejected", Box::new(|| Err(WorkerRequestError::Rejected))),
            ("gone", Box::new(|| Err(WorkerRequestError::WorkerGone))),
            (
                "too large",
                Box::new(move || {
                    Ok(json!({
                        "tools": [{"name": huge, "inputSchema": {"type": "object"}}],
                        "content": [{"type": "text", "text": huge}],
                    }))
                }),
            ),
            (
                "malformed",
                Box::new(|| Ok(json!({"tools": "worker-secret", "content": "worker-secret"}))),
            ),
        ];
        for (label, reply) in cases {
            let reply = Arc::new(reply);
            let mut fake = fake(Some("sess-1"), move || Some(reply()));

            let (status, body) = rpc(&fake.app, call_request(json!(3))).await;
            assert_eq!(status, StatusCode::OK, "{label}");
            let result = &body["result"];
            assert_eq!(result["isError"], json!(true), "{label}: {body}");
            assert_eq!(
                result["content"],
                json!([{"type": "text", "text": CALL_FAILED_TEXT}]),
                "{label}"
            );

            let (_, body) = rpc(&fake.app, list_request()).await;
            assert!(body["result"].is_null(), "{label}: {body}");
            assert!(body["error"]["code"].is_i64(), "{label}: {body}");
            assert!(!body.to_string().contains("worker-secret"), "{label}");

            // A timed-out request is abandoned, so the worker is told to stop it.
            let cancels: Vec<Note> = drain(&mut fake.calls)
                .into_iter()
                .filter(|_| label == "timeout")
                .map(|(id, _, _)| {
                    (
                        "mcp.cancel".to_string(),
                        json!({"server": "srv", "session_id": "sess-1", "request_id": id}),
                    )
                })
                .collect();
            assert_eq!(drain(&mut fake.notes), cancels, "{label}");
        }
    }

    #[tokio::test]
    async fn dropping_a_request_mid_call_sends_one_cancel() {
        let Fake {
            app,
            mut calls,
            mut notes,
        } = fake(Some("sess-1"), || None);
        let request = tokio::spawn({
            let app = app.clone();
            async move { rpc(&app, call_request(json!("req-7"))).await }
        });
        let (id, _, _) = tokio::time::timeout(DEADLINE, calls.recv())
            .await
            .expect("the call reaches the worker")
            .expect("the worker is still referenced");
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        drop(app);

        let mut seen = Vec::new();
        // Every handler holding the worker finishes, closing the channel.
        while let Some(note) = tokio::time::timeout(DEADLINE, notes.recv())
            .await
            .expect("the cancelled handler finishes")
        {
            seen.push(note);
        }
        assert!(id.starts_with("h-fake-"), "{id}");
        assert_eq!(
            seen,
            [(
                "mcp.cancel".to_string(),
                json!({"server": "srv", "session_id": "sess-1", "request_id": id}),
            )],
            "the cancel names the worker request, not the client's JSON-RPC id"
        );
    }
}
