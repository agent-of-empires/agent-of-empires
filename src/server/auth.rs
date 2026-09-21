//! Token-based authentication middleware for the web dashboard.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::AppState;

/// Constant-time string comparison to prevent timing attacks on token values.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Resolve the real client IP, trusting X-Forwarded-For only from loopback
/// (i.e., only when the request came through the cloudflared proxy).
pub(crate) fn resolve_client_ip(
    socket_addr: SocketAddr,
    headers: &axum::http::HeaderMap,
) -> IpAddr {
    let socket_ip = socket_addr.ip();
    if socket_ip.is_loopback() {
        if let Some(cf_ip) = headers.get("cf-connecting-ip") {
            if let Ok(ip_str) = cf_ip.to_str() {
                if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
        if let Some(xff) = headers.get("x-forwarded-for") {
            if let Ok(xff_str) = xff.to_str() {
                if let Some(last) = xff_str.rsplit(',').next() {
                    if let Ok(ip) = last.trim().parse::<IpAddr>() {
                        return ip;
                    }
                }
            }
        }
    }
    socket_ip
}

/// Whether `client_ip` should be treated as a same-host caller that already passes the
/// filesystem-permission trust boundary.
fn is_local_trusted(client_ip: IpAddr) -> bool {
    client_ip.is_loopback()
}

/// Build a Set-Cookie header value with optional Secure flag for HTTPS tunnels.
fn build_cookie(token: &str, secure: bool, max_age_secs: u64) -> String {
    let mut cookie = format!(
        "aoe_token={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        token, max_age_secs
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Write the `aoe_token` Set-Cookie and the companion `x-aoe-token` header into a
/// response's header map.
fn write_token_headers(
    headers: &mut axum::http::HeaderMap,
    token: &str,
    behind_tunnel: bool,
    max_age_secs: u64,
) {
    let cookie = build_cookie(token, behind_tunnel, max_age_secs);
    headers.append(
        header::SET_COOKIE,
        cookie.parse().expect("cookie format must be valid"),
    );
    if let Ok(value) = token.parse() {
        headers.insert("x-aoe-token", value);
    }
}

/// Attach both the Set-Cookie and X-Aoe-Token headers to a response.
async fn attach_token_headers(response: &mut Response, state: &AppState) {
    let Some(current) = state.token_manager.current_token().await else {
        return;
    };
    let max_age = state.token_manager.lifetime_secs().await;
    write_token_headers(
        response.headers_mut(),
        &current,
        state.behind_tunnel,
        max_age,
    );
}

/// Extract all token candidates from the request (cookie, query parameter, and
/// Authorization header).
fn extract_tokens(request: &Request) -> Vec<(&str, TokenSource)> {
    let mut tokens = Vec::new();

    // Check cookie
    if let Some(cookie_header) = request.headers().get(header::COOKIE) {
        if let Ok(cookie_str) = cookie_header.to_str() {
            for cookie in cookie_str.split(';') {
                let cookie = cookie.trim();
                if let Some(value) = cookie.strip_prefix("aoe_token=") {
                    tokens.push((value, TokenSource::Cookie));
                }
            }
        }
    }

    // Check query parameter
    if let Some(query) = request.uri().query() {
        for param in query.split('&') {
            if let Some(value) = param.strip_prefix("token=") {
                tokens.push((value, TokenSource::QueryParam));
            }
        }
    }

    // Check Authorization: Bearer header
    if let Some(auth_header) = request.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(value) = auth_str.strip_prefix("Bearer ") {
                tokens.push((value.trim(), TokenSource::Bearer));
            }
        }
    }

    tokens
}

/// Extract all WebSocket sub-protocol values from the request.
fn extract_ws_protocols(request: &Request) -> Vec<String> {
    let mut protocols = Vec::new();
    if let Some(header) = request.headers().get("sec-websocket-protocol") {
        if let Ok(proto_str) = header.to_str() {
            for proto in proto_str.split(',') {
                let trimmed = proto.trim();
                if !trimmed.is_empty() {
                    protocols.push(trimmed.to_string());
                }
            }
        }
    }
    protocols
}

/// Strip a possible trailing slash from a path so suffix matches are not bypassed by
/// `/api/sessions/123/acp/prompt/` (axum routes both forms to the same handler).
fn normalize_path(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

/// Whether a request path is exempt from the passphrase session + device-binding check.
fn is_login_session_exempt(path: &str) -> bool {
    path == "/"
        || path == "/login"
        || path.starts_with("/session/")
        || path == "/api/login"
        || path == "/api/login/status"
        || path == "/api/logout"
        || path == "/theme-bootstrap.js"
        || path.starts_with("/assets/")
        || path == "/manifest.json"
        || path == "/sw.js"
        || path.starts_with("/icon-")
        || path.starts_with("/fonts/")
}

/// Whether to append a sliding-window refresh of the `aoe_session` cookie on the response
/// for a session-authenticated request.
fn should_refresh_session_cookie(path: &str) -> bool {
    !is_login_session_exempt(path)
}

/// Decision for the post-token branch of `auth_middleware`.
#[derive(Debug, PartialEq, Eq)]
enum PostTokenAuthAction {
    /// Pass the request through to the next layer.
    Bypass,
    /// Return a `login_required` response so the SPA pops the
    /// passphrase prompt (or redirects to `/login` for HTML).
    RequireLogin,
}

/// Resolve the post-token branch decision.
fn post_token_auth_action(
    login_enabled: bool,
    login_exempt: bool,
    client_ip: IpAddr,
) -> PostTokenAuthAction {
    if !login_enabled || login_exempt {
        return PostTokenAuthAction::Bypass;
    }
    if is_local_trusted(client_ip) {
        PostTokenAuthAction::Bypass
    } else {
        PostTokenAuthAction::RequireLogin
    }
}

/// Decision for the entry of `run_passphrase_wall`.
#[derive(Debug, PartialEq, Eq)]
enum PassphraseWallEntryAction {
    /// Path is in the login-bootstrap allow-list.
    BypassExempt,
    /// Caller is on loopback and no external ingress fronts the daemon.
    BypassLoopback,
    /// Run the full session + device-binding + elevation flow.
    Continue,
}

/// Resolve the entry decision for `run_passphrase_wall`.
fn passphrase_wall_entry_action(
    path: &str,
    client_ip: IpAddr,
    behind_ingress: bool,
) -> PassphraseWallEntryAction {
    if is_login_session_exempt(path) {
        return PassphraseWallEntryAction::BypassExempt;
    }
    if !behind_ingress && is_local_trusted(client_ip) {
        return PassphraseWallEntryAction::BypassLoopback;
    }
    PassphraseWallEntryAction::Continue
}

/// Emit the passphrase-only loopback-bypass event.
fn log_loopback_bypass_passphrase(client_ip: IpAddr, path: &str) {
    tracing::debug!(
        target: "auth.passphrase",
        ip = %client_ip,
        path = %path,
        "loopback bypass: skipping passphrase factor in passphrase-only mode"
    );
}

/// Emit the token-mode loopback-bypass event.
fn log_loopback_bypass_token(client_ip: IpAddr, path: &str) {
    tracing::debug!(
        target: "auth",
        ip = %client_ip,
        path = %path,
        "loopback bypass: valid token + loopback peer; skipping passphrase factor"
    );
}

/// Whether a request path + method needs an elevated login session (step-up auth, 15-minute
/// passphrase confirmation window).
fn requires_elevation(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;

    let path = normalize_path(path);

    if method == Method::GET || method == Method::HEAD {
        return false;
    }

    // Settings + profile mutations.
    if path == "/api/settings" && method == Method::PATCH {
        return true;
    }
    if path == "/api/default-profile" && method == Method::PATCH {
        return true;
    }
    if path == "/api/profiles" && method == Method::POST {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/api/profiles/") {
        // Per-profile writes.
        if method == Method::PATCH && (rest.ends_with("/settings") || rest.ends_with("/settings/"))
        {
            return false;
        }
        return true;
    }

    // Device / login-session management.
    if path == "/api/login/logout-all" && method == Method::POST {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/api/login/sessions/") {
        if method == Method::DELETE && !rest.is_empty() {
            return true;
        }
    }

    false
}

/// Strip the leading `<prefix>.` from a subprotocol value when present, returning the
/// suffix.
fn strip_ws_prefix<'a>(proto: &'a str, prefix: &str) -> Option<&'a str> {
    let with_dot = proto.strip_prefix(prefix)?;
    with_dot.strip_prefix('.')
}

/// Extract the device binding secret presented by the client.
pub(crate) fn extract_device_binding(request: &Request) -> Option<Vec<u8>> {
    if let Some(value) = request.headers().get("x-aoe-device-binding") {
        if let Ok(s) = value.to_str() {
            if let Some(bytes) = super::login::decode_binding_secret(s) {
                return Some(bytes);
            }
        }
    }
    for proto in extract_ws_protocols(request) {
        if let Some(secret) = strip_ws_prefix(&proto, "aoe-device") {
            if let Some(bytes) = super::login::decode_binding_secret(secret) {
                return Some(bytes);
            }
        }
    }
    None
}

#[derive(Debug, PartialEq)]
enum TokenSource {
    Cookie,
    QueryParam,
    WebSocketProtocol,
    Bearer,
}

/// Request extension carrying the SHA-256 hash of the bearer token that authenticated this
/// request.
#[derive(Clone, Copy, Debug)]
pub struct AuthenticatedTokenHash(pub [u8; 32]);

/// Request extension carrying the login session id used to authenticate the current
/// request.
#[derive(Clone, Debug)]
pub struct AuthenticatedSession(pub String);

/// Request extension marking a request whose resolved client IP is loopback (see
/// `is_local_trusted`).
#[derive(Clone, Copy, Debug)]
pub struct LoopbackTrusted;

/// Pure elevation decision, extracted so the matrix is unit-testable without standing up
/// `AppState`.
fn elevation_verdict(
    login_enabled: bool,
    loopback_trusted: bool,
    session_elevated: Option<bool>,
) -> bool {
    if !login_enabled || loopback_trusted {
        return true;
    }
    session_elevated.unwrap_or(false)
}

/// Shared resolver for handler-side (body-shape) elevation gates.
pub(crate) async fn handler_elevated(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
    loopback_trusted: bool,
) -> bool {
    let session_elevated = match session {
        Some(AuthenticatedSession(id)) => Some(state.login_manager.is_elevated(id).await),
        None => None,
    };
    elevation_verdict(
        state.login_manager.is_enabled(),
        loopback_trusted,
        session_elevated,
    )
}

/// Passphrase login wall used when the token gate is disabled (`--auth=passphrase`).
async fn run_passphrase_wall(
    state: &AppState,
    request: Request,
    client_ip: IpAddr,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    match passphrase_wall_entry_action(&path, client_ip, state.behind_tunnel) {
        PassphraseWallEntryAction::BypassExempt => return next.run(request).await,
        PassphraseWallEntryAction::BypassLoopback => {
            log_loopback_bypass_passphrase(client_ip, &path);
            return next.run(request).await;
        }
        PassphraseWallEntryAction::Continue => {}
    }

    let session_id = super::login::extract_login_session(&request);
    let presented_binding = extract_device_binding(&request);

    let has_valid_session = match (&session_id, &presented_binding) {
        (Some(id), Some(binding)) => state.login_manager.validate_session(id, binding).await,
        _ => false,
    };

    if !has_valid_session {
        if path.starts_with("/api/") || path.contains("/ws") {
            tracing::warn!(
                target: "auth",
                ip = %client_ip,
                path = %path,
                had_session_cookie = session_id.is_some(),
                had_device_binding = presented_binding.is_some(),
                "passphrase wall: rejecting api/ws with 401"
            );
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "error": "login_required",
                    "message": "Passphrase login required"
                })),
            )
                .into_response();
        }
        return axum::response::Redirect::temporary("/login").into_response();
    }

    let session_id = session_id.expect("valid session implies session_id exists");

    if requires_elevation(&method, &path) && !state.login_manager.is_elevated(&session_id).await {
        tracing::info!(
            target: "auth.passphrase",
            ip = %client_ip,
            path = %path,
            "passphrase wall: sensitive route required elevation; returning 403"
        );
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "elevation_required",
                "message": "Re-enter the passphrase to continue"
            })),
        )
            .into_response();
    }

    let mut request = request;
    request
        .extensions_mut()
        .insert(AuthenticatedSession(session_id.clone()));

    let mut response = next.run(request).await;

    // Refresh login session cookie (sliding window).
    let login_cookie = super::login::build_login_cookie(&session_id, state.behind_tunnel);
    response.headers_mut().append(
        header::SET_COOKIE,
        login_cookie.parse().expect("cookie format must be valid"),
    );

    response
}

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let client_ip = resolve_client_ip(addr, request.headers());

    // Mark same-host callers before any auth branch so handler-side elevation gates see the
    // #1168 carve-out no matter which path (token, session, passphrase wall, loopback
    // bypass) handled the request.
    let wall_covers_loopback = state.behind_tunnel
        && state.token_manager.is_no_auth().await
        && state.login_manager.is_enabled();
    if !wall_covers_loopback && is_local_trusted(client_ip) {
        request.extensions_mut().insert(LoopbackTrusted);
    }

    // Trace structured view ws specifically so we can see whether the browser ever reached
    // the server when the structured view live updates get stuck.
    if request.uri().path().contains("/acp/ws") {
        let token_sources: Vec<&'static str> = extract_tokens(&request)
            .iter()
            .map(|(_, src)| match src {
                TokenSource::Cookie => "cookie",
                TokenSource::QueryParam => "query",
                TokenSource::Bearer => "bearer",
                TokenSource::WebSocketProtocol => "ws-proto",
            })
            .collect();
        let ws_protocols = extract_ws_protocols(&request);
        tracing::debug!(
            target: "auth",
            ip = %client_ip,
            token_sources = ?token_sources,
            ws_protocol_count = ws_protocols.len(),
            "auth_middleware entered for structured view ws"
        );
    }

    // Token gate disabled (--auth=none or --auth=passphrase).
    if state.token_manager.is_no_auth().await {
        static NO_AUTH_LOGGED: std::sync::Once = std::sync::Once::new();
        if state.login_manager.is_enabled() {
            NO_AUTH_LOGGED.call_once(|| {
                tracing::info!(
                    target: "auth.token",
                    "token gate disabled (--auth=passphrase); passphrase login wall remains active"
                );
            });
            request
                .extensions_mut()
                .insert(AuthenticatedTokenHash([0u8; 32]));
            return run_passphrase_wall(&state, request, client_ip, next).await;
        }
        NO_AUTH_LOGGED.call_once(|| {
            tracing::info!(
                target: "auth.token",
                "running in no-auth mode; requests pass through without token check"
            );
        });
        request
            .extensions_mut()
            .insert(AuthenticatedTokenHash([0u8; 32]));
        return next.run(request).await;
    }

    // Rate limit check BEFORE token validation
    if let Some(remaining_secs) = state.rate_limiter.check_locked(client_ip).await {
        tracing::warn!(
            target: "auth.rate_limit",
            ip = %client_ip,
            remaining_secs,
            "rejecting request from locked-out IP"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining_secs.to_string())],
            axum::Json(serde_json::json!({
                "error": "rate_limited",
                "message": format!(
                    "Too many failed attempts. Try again in {} seconds.",
                    remaining_secs
                )
            })),
        )
            .into_response();
    }

    // Steady-state path.
    let login_enabled = state.login_manager.is_enabled();
    let presented_session_id = if login_enabled {
        super::login::extract_login_session(&request)
    } else {
        None
    };
    let presented_binding = if login_enabled {
        extract_device_binding(&request)
    } else {
        None
    };
    let session_valid = if login_enabled {
        match (&presented_session_id, &presented_binding) {
            (Some(id), Some(b)) => state.login_manager.validate_session(id, b).await,
            _ => false,
        }
    } else {
        false
    };

    if session_valid {
        let session_id = presented_session_id.expect("session_valid implies session_id exists");
        return handle_session_authenticated(&state, client_ip, request, next, session_id).await;
    }

    // Token-check path.
    let mut matched_source = None;
    let mut needs_upgrade = false;
    let mut matched_token_hash: Option<[u8; 32]> = None;

    for (token_value, source) in extract_tokens(&request) {
        let (valid, upgrade) = state.token_manager.validate(token_value).await;
        if valid {
            matched_source = Some(source);
            needs_upgrade = upgrade;
            matched_token_hash = Some(super::push::sha256_token(token_value));
            break;
        }
    }

    // WebSocket sub-protocol fallback.
    if matched_source.is_none() {
        for proto in extract_ws_protocols(&request) {
            let candidate = strip_ws_prefix(&proto, "aoe-token").unwrap_or(&proto);
            let (valid, upgrade) = state.token_manager.validate(candidate).await;
            if valid {
                matched_source = Some(TokenSource::WebSocketProtocol);
                needs_upgrade = upgrade;
                matched_token_hash = Some(super::push::sha256_token(candidate));
                break;
            }
        }
    }

    let Some(source) = matched_source else {
        // No valid token and no valid session.
        let path = request.uri().path();
        let is_api_or_ws = path.starts_with("/api/") || path.contains("/ws");
        if !is_api_or_ws {
            return next.run(request).await;
        }
        let locked = state.rate_limiter.record_failure(client_ip).await;
        let reason =
            if extract_tokens(&request).is_empty() && extract_ws_protocols(&request).is_empty() {
                "missing"
            } else {
                "invalid"
            };
        tracing::warn!(
            target: "auth.middleware",
            ip = %client_ip,
            path = %path,
            locked = locked,
            reason = %reason,
            "auth rejected"
        );
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "unauthorized",
                "message": "Invalid or missing auth token"
            })),
        )
            .into_response();
    };

    // Token valid: record success, stamp owner, record device.
    state.rate_limiter.record_success(client_ip).await;
    tracing::trace!(
        target: "auth.middleware",
        ip = %client_ip,
        path = %request.uri().path(),
        source = ?source,
        "auth accepted via token (bootstrap)"
    );
    if let Some(hash) = matched_token_hash {
        request
            .extensions_mut()
            .insert(AuthenticatedTokenHash(hash));
    }
    let path = request.uri().path().to_string();
    let should_attach_token =
        matches!(source, TokenSource::QueryParam | TokenSource::Bearer) || needs_upgrade;

    // When login is enabled, a valid token alone is not enough for non-bootstrap paths.
    let login_exempt = is_login_session_exempt(&path);
    match post_token_auth_action(login_enabled, login_exempt, client_ip) {
        PostTokenAuthAction::Bypass => {
            if login_enabled && !login_exempt && is_local_trusted(client_ip) {
                log_loopback_bypass_token(client_ip, &path);
            }
        }
        PostTokenAuthAction::RequireLogin => {
            tracing::warn!(
                target: "auth",
                ip = %client_ip,
                path = %path,
                had_session_cookie = presented_session_id.is_some(),
                had_device_binding = presented_binding.is_some(),
                "valid token but no session on non-login-exempt path; returning login_required"
            );
            if path.starts_with("/api/") || path.contains("/ws") {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({
                        "error": "login_required",
                        "message": "Passphrase login required"
                    })),
                )
                    .into_response();
            } else {
                let mut response = axum::response::Redirect::temporary("/login").into_response();
                if should_attach_token {
                    attach_token_headers(&mut response, &state).await;
                }
                return response;
            }
        }
    }

    // Bootstrap path (login enabled + /login, /api/login, etc.), token-only mode, or
    // loopback bypass per the match above.
    let mut response = next.run(request).await;
    if should_attach_token {
        attach_token_headers(&mut response, &state).await;
    }
    response
}

/// Steady-state handler for a bound device.
async fn handle_session_authenticated(
    state: &Arc<AppState>,
    client_ip: IpAddr,
    mut request: Request,
    next: Next,
    session_id: String,
) -> Response {
    state.rate_limiter.record_success(client_ip).await;
    tracing::trace!(
        target: "auth.middleware",
        ip = %client_ip,
        path = %request.uri().path(),
        "auth accepted via session+binding"
    );

    let owner_hash = match state.token_manager.current_token().await {
        Some(t) => super::push::sha256_token(&t),
        None => [0u8; 32],
    };
    request
        .extensions_mut()
        .insert(AuthenticatedTokenHash(owner_hash));

    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // Loopback callers skip the step-up gate even with a session.
    if requires_elevation(&method, &path)
        && request.extensions().get::<LoopbackTrusted>().is_none()
        && !state.login_manager.is_elevated(&session_id).await
    {
        tracing::info!(
            target: "auth.passphrase",
            ip = %client_ip,
            path = %path,
            "sensitive route required elevation; returning 403 elevation_required"
        );
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "elevation_required",
                "message": "Re-enter the passphrase to continue"
            })),
        )
            .into_response();
    }

    request
        .extensions_mut()
        .insert(AuthenticatedSession(session_id.clone()));

    let mut response = next.run(request).await;

    if should_refresh_session_cookie(&path) {
        let login_cookie = super::login::build_login_cookie(&session_id, state.behind_tunnel);
        response.headers_mut().append(
            header::SET_COOKIE,
            login_cookie.parse().expect("cookie format must be valid"),
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    // Records (level, target) for every event so the loopback-bypass
    // helpers' log level is assertable without standing up the full
    // axum middleware (AppState is too heavy to build in a unit test).
    #[derive(Clone, Default)]
    struct LevelCapture(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LevelCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = event.metadata();
            self.0
                .lock()
                .unwrap()
                .push((*meta.level(), meta.target().to_string()));
        }
    }

    fn capture_events(f: impl FnOnce()) -> Vec<(tracing::Level, String)> {
        use tracing_subscriber::layer::SubscriberExt;
        let capture = LevelCapture::default();
        let events = capture.0.clone();
        let subscriber = tracing_subscriber::registry::Registry::default().with(capture);
        tracing::subscriber::with_default(subscriber, f);
        let recorded = events.lock().unwrap();
        recorded.clone()
    }

    fn build_request_with_headers(headers: Vec<(&'static str, &'static str)>) -> Request {
        let mut builder = Request::builder().uri("/api/sessions");
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// #1647: a loopback bypass is routine, so it must not be louder than debug.
    #[test]
    fn loopback_bypass_logs_at_debug() {
        let at = ip("127.0.0.1");
        let debug = |target: &str| vec![(tracing::Level::DEBUG, target.to_string())];
        assert_eq!(
            capture_events(|| log_loopback_bypass_passphrase(at, "/api/sessions")),
            debug("auth.passphrase")
        );
        assert_eq!(
            capture_events(|| log_loopback_bypass_token(at, "/api/sessions")),
            debug("auth")
        );
    }

    #[test]
    fn constant_time_eq_compares_whole_strings() {
        for (a, b) in [("abc123", "abc123"), ("", "")] {
            assert!(constant_time_eq(a, b), "{a:?} == {b:?}");
        }
        for (a, b) in [
            ("abc123", "abc124"),
            ("abc123", "xyz789"),
            ("short", "longer_string"),
            ("abc", "ab"),
            ("", "x"),
            ("x", ""),
        ] {
            assert!(!constant_time_eq(a, b), "{a:?} != {b:?}");
        }
    }

    /// X-Forwarded-For is only trusted from loopback, i.e. only behind the proxy.
    #[test]
    fn resolve_client_ip_trusts_forwarding_headers_only_from_loopback() {
        let from = |socket: &str, headers: &[(&str, &str)]| {
            let mut map = axum::http::HeaderMap::new();
            for (k, v) in headers {
                let name = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
                map.insert(name, v.parse().unwrap());
            }
            resolve_client_ip(socket.parse().unwrap(), &map)
        };
        let cf = ("cf-connecting-ip", "203.0.113.50");
        let xff = |v: &'static str| ("x-forwarded-for", v);

        let loopback = "127.0.0.1:12345";
        assert_eq!(
            from(loopback, &[cf, xff("10.0.0.1")]),
            ip("203.0.113.50"),
            "cf-connecting-ip wins over xff"
        );
        assert_eq!(
            from(loopback, &[xff("spoofed.by.client, 203.0.113.50")]),
            ip("203.0.113.50"),
            "xff uses the last hop, not the client-spoofable head"
        );
        assert_eq!(from(loopback, &[]), ip("127.0.0.1"));
        assert_eq!(
            from(loopback, &[xff("not-an-ip")]),
            ip("127.0.0.1"),
            "malformed xff falls back to the socket"
        );
        assert_eq!(
            from("192.168.1.100:12345", &[xff("10.0.0.1")]),
            ip("192.168.1.100"),
            "a remote peer cannot forge its own origin"
        );
    }

    #[test]
    fn is_local_trusted_recognizes_loopback() {
        for local in ["127.0.0.1", "::1"] {
            assert!(is_local_trusted(ip(local)), "{local}");
        }
        for remote in ["192.168.1.50", "100.64.0.5", "203.0.113.10"] {
            assert!(!is_local_trusted(ip(remote)), "{remote}");
        }
    }

    /// Full matrix of the handler-side elevation decision.
    #[test]
    fn elevation_verdict_matrix() {
        // (login_enabled, loopback, session elevated, expected)
        let cases = [
            // Login disabled: elevation does not exist as a concept.
            (false, false, None, true),
            (false, true, None, true),
            // Loopback is trusted whatever the session says.
            (true, true, None, true),
            (true, true, Some(false), true),
            (true, true, Some(true), true),
            // Remote with login enabled: only an elevated session passes.
            (true, false, None, false),
            (true, false, Some(false), false),
            (true, false, Some(true), true),
        ];
        for (enabled, loopback, elevated, want) in cases {
            assert_eq!(
                elevation_verdict(enabled, loopback, elevated),
                want,
                "enabled={enabled} loopback={loopback} elevated={elevated:?}"
            );
        }
    }

    /// Per-row coverage of the post-token branch policy from #1168. The remote +
    /// non-bootstrap row is the threat passphrase auth exists to mitigate (a leaked
    /// token used from off-box); a real reverse proxy is covered because
    /// `resolve_client_ip` hands us the forwarded remote IP, not the loopback socket.
    #[test]
    fn post_token_auth_action_matrix() {
        use PostTokenAuthAction::*;
        // (login_enabled, login_exempt, client ip, expected)
        let cases = [
            (false, false, "127.0.0.1", Bypass),
            (false, false, "100.64.0.5", Bypass),
            (true, true, "127.0.0.1", Bypass),
            (true, true, "100.64.0.5", Bypass),
            (true, false, "127.0.0.1", Bypass),
            (true, false, "::1", Bypass),
            (true, false, "100.64.0.5", RequireLogin),
        ];
        for (enabled, exempt, client, want) in cases {
            assert_eq!(
                post_token_auth_action(enabled, exempt, ip(client)),
                want,
                "enabled={enabled} exempt={exempt} ip={client}"
            );
        }
    }

    /// Per-row coverage of the passphrase-wall entry policy added in #1525. The
    /// login-bootstrap allow-list wins regardless of IP or ingress, otherwise nobody
    /// could reach the wall to get past it; #3843 is the ingress column.
    #[test]
    fn passphrase_wall_entry_action_matrix() {
        use PassphraseWallEntryAction::*;
        // (path, client ip, behind_ingress, expected)
        let cases = [
            ("/api/sessions", "127.0.0.1", false, BypassLoopback),
            ("/sessions/abc/acp/ws", "127.0.0.1", false, BypassLoopback),
            ("/api/settings", "::1", false, BypassLoopback),
            ("/api/sessions", "100.64.0.5", false, Continue),
            ("/sessions/abc/acp/ws", "100.64.0.5", false, Continue),
            ("/login", "100.64.0.5", false, BypassExempt),
            ("/api/login", "100.64.0.5", false, BypassExempt),
            ("/assets/index.css", "127.0.0.1", false, BypassExempt),
            ("/api/sessions", "127.0.0.1", true, Continue),
            ("/sessions/abc/acp/ws", "::1", true, Continue),
            ("/api/login", "127.0.0.1", true, BypassExempt),
        ];
        for (path, client, ingress, want) in cases {
            assert_eq!(
                passphrase_wall_entry_action(path, ip(client), ingress),
                want,
                "path={path} ip={client} ingress={ingress}"
            );
        }
    }

    #[test]
    fn build_cookie_carries_secure_only_over_tls() {
        let insecure = build_cookie("mytoken", false, 14400);
        for needle in [
            "aoe_token=mytoken",
            "HttpOnly",
            "SameSite=Strict",
            "Max-Age=14400",
        ] {
            assert!(insecure.contains(needle), "{insecure:?} lacks {needle}");
        }
        assert!(!insecure.contains("Secure"));
        assert!(build_cookie("mytoken", true, 14400).contains("Secure"));
    }

    #[test]
    fn extract_tokens_prefers_the_cookie_and_ignores_other_schemes() {
        let cookie_and_bearer = build_request_with_headers(vec![
            ("cookie", "aoe_token=cookie_tok"),
            ("authorization", "Bearer bearer_tok"),
        ]);
        assert_eq!(
            extract_tokens(&cookie_and_bearer),
            vec![
                ("cookie_tok", TokenSource::Cookie),
                ("bearer_tok", TokenSource::Bearer),
            ]
        );

        let padded = build_request_with_headers(vec![("authorization", "Bearer   padded  ")]);
        assert_eq!(
            extract_tokens(&padded),
            vec![("padded", TokenSource::Bearer)]
        );

        let basic = build_request_with_headers(vec![("authorization", "Basic dXNlcjpwYXNz")]);
        assert!(extract_tokens(&basic).is_empty());
    }

    #[test]
    fn strip_ws_prefix_requires_the_dot_separator() {
        assert_eq!(strip_ws_prefix("aoe-token.abc", "aoe-token"), Some("abc"));
        assert_eq!(strip_ws_prefix("aoe-device.xyz", "aoe-device"), Some("xyz"));
        // No leading dot -> not a prefixed value, just a coincidentally matching string.
        assert_eq!(strip_ws_prefix("aoe-tokenabc", "aoe-token"), None);
        assert_eq!(strip_ws_prefix("graphql-ws", "aoe-token"), None);
    }

    #[test]
    fn extract_device_binding_reads_header_or_ws_subprotocol() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let raw = [0xAB; 32];
        let encoded = URL_SAFE_NO_PAD.encode(raw);
        let from_header = build_request_with_headers(vec![(
            "x-aoe-device-binding",
            Box::leak(encoded.clone().into_boxed_str()),
        )]);
        assert_eq!(
            extract_device_binding(&from_header).as_deref(),
            Some(&raw[..])
        );

        let proto = format!("aoe-token.tok123, aoe-device.{encoded}");
        let from_ws = build_request_with_headers(vec![(
            "sec-websocket-protocol",
            Box::leak(proto.into_boxed_str()),
        )]);
        assert_eq!(extract_device_binding(&from_ws).as_deref(), Some(&raw[..]));

        assert!(extract_device_binding(&build_request_with_headers(vec![])).is_none());
        let malformed = build_request_with_headers(vec![(
            "x-aoe-device-binding",
            "not-base64-and-wrong-length",
        )]);
        assert!(extract_device_binding(&malformed).is_none());
    }

    /// Regression test for the token-cookie clobber bug: the middleware appends its
    /// cookie instead of replacing whatever the handler already set.
    #[test]
    fn write_token_headers_preserves_prior_set_cookie() {
        use axum::http::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert(
            header::SET_COOKIE,
            HeaderValue::from_static(
                "aoe_session=abc; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000",
            ),
        );
        write_token_headers(&mut headers, "tok123", false, 14400);

        let cookies: Vec<String> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        assert_eq!(
            cookies.len(),
            2,
            "both Set-Cookie values must survive: {cookies:?}"
        );
        for needle in ["aoe_session=abc", "aoe_token=tok123"] {
            assert!(cookies.iter().any(|c| c.contains(needle)), "{cookies:?}");
        }
        assert_eq!(
            headers.get("x-aoe-token").and_then(|v| v.to_str().ok()),
            Some("tok123")
        );
    }

    /// The login-bootstrap allow-list, and the sliding-window refresh that tracks it.
    /// A refresh on an exempt path would clobber the logout that just cleared the
    /// cookie, so the two must stay in lockstep apart from `/api/login/elevate`,
    /// which is session-gated and so does slide the window.
    #[test]
    fn login_session_exempt_paths_are_not_refreshed() {
        let exempt = [
            "/",
            "/login",
            "/session/abc",
            "/session/2626c6af68754732",
            "/api/login",
            "/api/login/status",
            "/api/logout",
            // Static assets: pre-load the SPA shell before any auth.
            "/theme-bootstrap.js",
            "/assets/index.css",
            "/assets/index-abc123.js",
            "/manifest.json",
            "/sw.js",
            "/icon-192.png",
            "/fonts/inter.woff2",
        ];
        for path in exempt {
            assert!(is_login_session_exempt(path), "{path} is exempt");
            assert!(
                !should_refresh_session_cookie(path),
                "{path} is not refreshed"
            );
        }

        // Everything else stays gated, including real data/API and websocket attach
        // routes that can carry a device binding. `/api/login/foo` is not `/api/login`
        // and `/logins` is not `/login`.
        let gated = [
            "/api/sessions",
            "/api/sessions/abc/acp/ws",
            "/api/settings",
            "/api/login/elevate",
            "/sessions/abc/ws",
            "/api/sessions/abc/ws",
            "/api/login/foo",
            "/logins",
        ];
        for path in gated {
            assert!(!is_login_session_exempt(path), "{path} is gated");
            assert!(
                should_refresh_session_cookie(path),
                "{path} slides the window"
            );
        }
    }

    /// Pin the session lifetime to 30 days (#1167) and keep the cookie's advertised
    /// Max-Age equal to the server TTL, so browser and server agree on expiry.
    #[tokio::test]
    async fn login_session_lifetime_is_thirty_days_sliding() {
        use crate::server::login::{LoginManager, SESSION_LIFETIME};
        use std::time::Duration;

        assert_eq!(SESSION_LIFETIME, Duration::from_secs(30 * 24 * 60 * 60));

        let mgr = LoginManager::new(Some("test"));
        let binding = vec![0xAB; 32];
        let session_id = mgr
            .create_session(&binding, "127.0.0.1", "test-agent")
            .await;
        assert!(mgr.validate_session(&session_id, &binding).await);

        let cookie = super::super::login::build_login_cookie(&session_id, false);
        let expected = format!("Max-Age={}", SESSION_LIFETIME.as_secs());
        assert!(
            cookie.contains(&expected),
            "cookie {cookie:?} must advertise {expected}"
        );
    }

    /// The passphrase wall gates settings, profile management and device/login-session
    /// management. `PATCH /api/profiles/{name}/settings` is body-gated inside
    /// `update_profile_settings` instead, which re-issues the same 403 per leaf.
    #[test]
    fn requires_elevation_paths() {
        use axum::http::Method;

        let gated = [
            (Method::PATCH, "/api/settings"),
            // A trailing slash must not bypass the gate.
            (Method::PATCH, "/api/settings/"),
            (Method::PATCH, "/api/default-profile"),
            (Method::POST, "/api/profiles"),
            (Method::PATCH, "/api/profiles/work/rename"),
            (Method::DELETE, "/api/profiles/work"),
            (Method::POST, "/api/login/logout-all"),
            (Method::DELETE, "/api/login/sessions/abc123"),
        ];
        for (method, path) in gated {
            assert!(requires_elevation(&method, path), "{method} {path}");
        }

        let ungated = [
            (Method::PATCH, "/api/profiles/work/settings"),
            (Method::PATCH, "/api/profiles/work/settings/"),
            // Session traffic: attach, prompt, approve, spawn, delete.
            (Method::GET, "/api/sessions/abc/ws"),
            (Method::GET, "/api/sessions/abc/ws-readonly"),
            (Method::GET, "/sessions/abc/acp/ws"),
            (Method::POST, "/api/sessions/abc/acp/prompt"),
            (Method::POST, "/api/sessions/abc/acp/cancel"),
            (Method::POST, "/api/sessions/abc/acp/approvals/nonce1"),
            (Method::POST, "/api/sessions"),
            (Method::DELETE, "/api/sessions/abc"),
            (Method::POST, "/api/sessions/abc/send"),
            (Method::PATCH, "/api/sessions/abc"),
            (Method::POST, "/api/sessions/abc/ensure"),
            (Method::PATCH, "/api/sessions/abc/notifications"),
            (Method::DELETE, "/api/workspaces"),
            (Method::POST, "/api/git/clone"),
            (Method::POST, "/api/projects"),
            (Method::DELETE, "/api/projects/myproj"),
            (Method::PATCH, "/api/projects/myproj"),
            (Method::POST, "/api/push/subscribe"),
            (Method::POST, "/api/push/unsubscribe"),
            // Cosmetic UI state and the update banner grant no capability; the
            // handlers still enforce read_only.
            (Method::POST, "/api/app-state/web-tour-seen"),
            (Method::POST, "/api/app-state/dismiss-update"),
            (Method::PATCH, "/api/app-state/web-ui-state"),
            // Read-only GETs, even on settings and profile paths.
            (Method::GET, "/api/settings"),
            (Method::GET, "/api/profiles"),
            (Method::GET, "/api/profiles/work/settings"),
            (Method::GET, "/api/sessions"),
            (Method::GET, "/api/sessions/abc"),
            (Method::GET, "/api/devices"),
            // Out of scope entirely.
            (Method::GET, "/api/about"),
            (Method::POST, "/api/login"),
            (Method::POST, "/api/login/elevate"),
            (Method::POST, "/api/logout"),
        ];
        for (method, path) in ungated {
            assert!(!requires_elevation(&method, path), "{method} {path}");
        }
    }
}
