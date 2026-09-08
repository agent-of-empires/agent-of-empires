//! The axum router and the request span every handler is traced under.

use axum::Router;
use std::sync::Arc;
use tracing::Instrument;

use super::access::{access_policy, cityhall_gate, security_headers};
#[cfg(feature = "web")]
use super::assets::{serve_asset, serve_index, serve_public_file};
use super::state::AppState;
use crate::server::{acp_ws, api, auth, live_ws, login, push};

pub(super) fn build_router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, get, patch, post, put};

    let app = Router::new()
        // Explicit browser visibility heartbeat. Ordinary API requests do not
        // imply the dashboard is foregrounded, so they must not suppress push.
        .route("/api/presence", post(api::post_dashboard_presence))
        // Sessions
        .route(
            "/api/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        // Static segment; registered before /api/sessions/{id} so the
        // literal "search" never resolves as a session id. See #2515.
        .route("/api/sessions/search", get(api::search_sessions))
        .route("/api/recent-projects", get(api::get_recent_projects))
        .route(
            "/api/workspace-ordering",
            put(api::update_workspace_ordering),
        )
        // Atomic multi-session workspace delete (#2536): one call replaces the
        // web client's N-call fan-out over DELETE /api/sessions/{id}.
        .route("/api/workspaces", delete(api::delete_workspace))
        // Unified MCP management surface (#1996)
        .route("/api/mcp/servers", get(api::get_mcp_servers))
        .route(
            "/api/mcp/servers/{name}/resolve",
            post(api::resolve_mcp_conflict),
        )
        .route("/api/mcp/servers/{name}/keep", post(api::keep_mcp_server))
        .route("/api/mcp/servers/{name}/drop", post(api::drop_mcp_server))
        // Unified skills management surface (#3050).
        .route("/api/skills", get(api::list_skills).post(api::create_skill))
        // Static, so it wins over `/api/skills/{directory}` and a managed skill
        // may still be named "sync" (that route is PUT/DELETE only).
        .route("/api/skills/sync", post(api::sync_skills))
        .route("/api/skills/{source}/{directory}", get(api::read_skill))
        .route(
            "/api/skills/{source}/{directory}/adopt",
            post(api::adopt_skill),
        )
        .route(
            "/api/skills/{directory}",
            put(api::edit_skill).delete(api::delete_skill),
        )
        .route(
            "/api/sessions/{id}",
            patch(api::rename_session).delete(api::delete_session),
        )
        .route("/api/sessions/{id}/group", patch(api::update_session_group))
        .route(
            "/api/sessions/{id}/diff/files",
            get(api::session_diff_files),
        )
        .route("/api/sessions/{id}/diff/file", get(api::session_diff_file))
        .route("/api/sessions/{id}/file", get(api::session_file))
        .route(
            "/api/sessions/{id}/artifacts/{*path}",
            get(api::serve_session_artifact),
        )
        .route("/api/sessions/{id}/ensure", post(api::ensure_session))
        .route("/api/sessions/{id}/send", post(api::send_message))
        .route(
            "/api/sessions/{id}/paste-image",
            // A base64 screenshot blows past the global 1 MiB cap. 8 MiB
            // leaves headroom for the 5 MiB decoded cap (enforced in the
            // handler) plus base64's ~33% overhead and JSON framing.
            post(api::paste_image).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/api/sessions/{id}/output", get(api::read_output))
        .route(
            "/api/sessions/{id}/notifications",
            patch(api::update_session_notifications),
        )
        .route(
            "/api/sessions/{id}/diff-base",
            patch(api::update_session_diff_base),
        )
        .route(
            "/api/sessions/{id}/worktree-name",
            patch(api::set_worktree_name),
        )
        .route(
            "/api/sessions/{id}/projects",
            post(api::attach_session_project),
        )
        .route("/api/sessions/{id}/pin", patch(api::update_session_pin))
        .route("/api/sessions/{id}/color", patch(api::update_session_color))
        .route(
            "/api/sessions/{id}/archive",
            patch(api::update_session_archive),
        )
        .route(
            "/api/sessions/{id}/snooze",
            patch(api::update_session_snooze),
        )
        .route("/api/sessions/{id}/trash", post(api::trash_session))
        .route("/api/sessions/{id}/restore", post(api::restore_session))
        .route(
            "/api/sessions/{id}/unread",
            patch(api::update_session_unread),
        )
        .route("/api/sessions/{id}/stop", post(api::stop_session))
        .route(
            "/api/sessions/{id}/smart-rename",
            post(api::force_smart_rename),
        )
        .route("/api/sessions/{id}/summarize", post(api::summarize_session))
        .route("/api/sessions/{id}/start", post(api::start_session))
        .route(
            "/api/sessions/{id}/terminal",
            post(api::ensure_terminal).delete(api::kill_terminal),
        )
        .route(
            "/api/sessions/{id}/container-terminal",
            post(api::ensure_container_terminal),
        )
        // Agents
        .route("/api/agents", get(api::list_agents))
        // Profiles
        .route(
            "/api/profiles",
            get(api::list_profiles).post(api::create_profile),
        )
        .route("/api/profiles/{name}", delete(api::delete_profile))
        .route(
            "/api/profiles/{name}/settings",
            get(api::get_profile_settings).patch(api::update_profile_settings),
        )
        .route("/api/profiles/{name}/rename", patch(api::rename_profile))
        .route("/api/default-profile", patch(api::default_profile))
        .route("/api/filesystem/browse", get(api::browse_filesystem))
        .route("/api/filesystem/home", get(api::filesystem_home))
        .route("/api/git/branches", get(api::list_branches))
        .route("/api/git/is-repo", get(api::is_git_repo))
        .route("/api/git/clone", post(api::clone_repo))
        .route("/api/groups", get(api::list_groups))
        .route(
            "/api/projects",
            get(api::list_projects).post(api::create_project),
        )
        .route(
            "/api/projects/{name}",
            patch(api::update_project).delete(api::delete_project),
        )
        .route("/api/docker/status", get(api::docker_status))
        // Settings + themes
        .route(
            "/api/settings",
            get(api::get_settings).patch(api::update_settings),
        )
        .route("/api/settings/schema", get(api::get_settings_schema))
        .route("/api/settings/resolved", get(api::get_settings_resolved))
        // The CityHall config bundle an admin hands to CityHall. Blocked inside
        // a CityHall workspace by the handler itself (reads bypass
        // `cityhall_gate`).
        .route("/api/cityhall/bundle", get(api::get_cityhall_bundle))
        .route("/api/tips", get(api::get_tips))
        .route("/api/tips/show", post(api::set_show_tips))
        .route("/api/app-state/tip-seen", post(api::mark_tip_seen))
        // Plugin management. The enable/disable toggle gates on read-only +
        // elevation inside the handler.
        .route("/api/plugins", get(api::list_plugins))
        .route("/api/plugins/{id}/icon", get(api::serve_plugin_icon))
        .route("/api/plugins/commands", get(api::plugin_commands))
        .route(
            "/api/plugins/commands/{fqid}/invoke",
            post(api::invoke_plugin_command),
        )
        .route("/api/plugins/ui-state", get(api::plugin_ui_state))
        .route("/api/plugins/updates", get(api::plugin_updates))
        .route("/api/plugins/discover", get(api::plugin_discover))
        .route("/api/plugins/details", get(api::plugin_details))
        .route("/api/plugins/{id}/enabled", post(api::set_plugin_enabled))
        .route("/api/plugins/{id}/action", post(api::invoke_plugin_action))
        .route(
            "/api/plugins/{id}/settings/options/resolve",
            post(api::resolve_options),
        )
        .route(
            "/api/plugins/install/preview",
            post(api::preview_plugin_install),
        )
        .route("/api/plugins/install", post(api::start_plugin_install))
        .route(
            "/api/plugins/{id}/uninstall",
            post(api::start_plugin_uninstall),
        )
        .route("/api/plugins/jobs/{job_id}", get(api::plugin_job_status))
        .route(
            "/api/plugins/{id}/update/preview",
            get(api::plugin_update_preview),
        )
        .route(
            "/api/plugins/{id}/update/apply",
            post(api::apply_plugin_update),
        )
        .route(
            "/api/plugins/{id}/update/dismiss",
            post(api::dismiss_plugin_update),
        )
        .route(
            "/api/app-state/web-tour-seen",
            post(api::mark_web_tour_seen),
        )
        .route("/api/app-state/dismiss-update", post(api::dismiss_update))
        .route(
            "/api/app-state/web-ui-state",
            get(api::get_web_ui_state).patch(api::patch_web_ui_state),
        )
        .route(
            "/api/app-state/volume-ignores-globs-acknowledged",
            post(api::mark_volume_ignores_globs_acknowledged),
        )
        .route(
            "/api/sandbox/volume-ignores-preview",
            get(api::preview_volume_ignores_globs),
        )
        .route("/api/themes", get(api::list_themes))
        .route("/api/themes/{name}", get(api::get_resolved_theme))
        .route("/api/theme/current", get(api::get_current_theme))
        // Dedicated, non-elevated global-theme write: a cosmetic theme change
        // must not trip the passphrase wall on `PATCH /api/settings`.
        .route("/api/theme", patch(api::update_theme))
        .route("/api/sounds", get(api::list_sounds))
        .route("/api/sounds/file/{name}", get(api::serve_sound_file))
        // Push notifications
        .route("/api/push/status", get(push::get_status))
        .route(
            "/api/push/vapid-public-key",
            get(push::get_vapid_public_key),
        )
        .route("/api/push/subscribe", post(push::subscribe))
        .route("/api/push/unsubscribe", post(push::unsubscribe))
        .route("/api/push/test", post(push::test))
        // Login (second-factor auth)
        .route("/api/login", post(login::login_handler))
        .route("/api/login/elevate", post(login::elevate_handler))
        .route("/api/logout", post(login::logout_handler))
        .route("/api/login/status", get(login::login_status_handler))
        // Sign out every device (elevation-gated). See #1235.
        .route("/api/login/logout-all", post(login::logout_all_handler))
        // Revoke a single device's login session (elevation-gated).
        .route(
            "/api/login/sessions/{id}",
            delete(login::revoke_session_handler),
        )
        // Devices: the connected-devices view is backed by persisted
        // login sessions (#1235), not the old IP/UA request tracker.
        .route("/api/devices", get(login::devices_handler))
        // About (version, auth status, read-only state)
        .route("/api/about", get(api::get_about))
        // Update status (latest release, available flag)
        .route("/api/system/update-status", get(api::get_update_status))
        .route(
            "/api/log-level",
            get(api::get_log_level).patch(api::patch_log_level),
        )
        .route("/api/client-log", post(api::post_client_log))
        // Telemetry consent (browser manages opt-in via the daemon; it never
        // posts to the telemetry backend directly).
        .route("/api/telemetry/status", get(api::get_telemetry_status))
        .route("/api/telemetry/consent", post(api::set_telemetry_consent))
        .route("/api/telemetry/seen", post(api::post_telemetry_seen))
        .route(
            "/api/telemetry/structured-interaction",
            post(api::post_telemetry_structured_interaction),
        )
        // Terminal WebSockets (capture-streaming live view; the agent pane and
        // the paired host/container shells). The xterm PTY relay was removed.
        .route("/sessions/{id}/live-ws", get(live_ws::live_terminal_ws))
        .route(
            "/sessions/{id}/terminal/live-ws",
            get(live_ws::live_paired_terminal_ws),
        )
        .route(
            "/sessions/{id}/container-terminal/live-ws",
            get(live_ws::live_container_terminal_ws),
        );

    let app = app
        .route("/sessions/{id}/acp/ws", get(acp_ws::acp_ws))
        .route("/api/sessions/{id}/acp/spawn", post(api::spawn_acp))
        .route(
            "/api/sessions/{id}/acp/install-agent",
            post(api::install_agent),
        )
        .route("/api/sessions/{id}/acp", delete(api::shutdown_acp))
        .route(
            "/api/sessions/{id}/acp/switch-agent",
            post(api::switch_acp_agent),
        )
        .route(
            "/api/sessions/{id}/acp/prompt",
            // Prompt bodies carry inline base64 attachments, which blow
            // past the global 1 MiB cap. Raise the limit on this route
            // only; the server-side decoded-size caps in
            // `validate_attachments` are the real guard. 28 MiB leaves
            // headroom for the 20 MiB total decoded cap plus base64's
            // ~33% overhead and JSON framing. See #1000 / #965.
            post(api::acp_prompt).layer(axum::extract::DefaultBodyLimit::max(28 * 1024 * 1024)),
        )
        .route(
            "/api/sessions/{id}/acp/attachments/{attachment_id}",
            get(api::acp_attachment),
        )
        .route(
            "/api/sessions/{id}/acp/prompt/diff-comments",
            post(api::acp_prompt_diff_comments),
        )
        .route("/api/sessions/{id}/acp/cancel", post(api::acp_cancel))
        .route(
            "/api/sessions/{id}/acp/force_end_turn",
            post(api::acp_force_end_turn),
        )
        .route("/api/sessions/{id}/acp/files", get(api::acp_files))
        .route(
            "/api/sessions/{id}/acp/worker-log",
            get(api::acp_worker_log),
        )
        .route("/api/sessions/{id}/acp/replay", get(api::acp_replay))
        .route(
            "/api/sessions/{id}/acp/context-primer",
            get(api::acp_context_primer),
        )
        .route("/api/sessions/{id}/acp/mode", post(api::acp_set_mode))
        .route(
            "/api/sessions/{id}/acp/config-option",
            post(api::acp_set_config_option),
        )
        .route("/api/sessions/{id}/acp/enable", post(api::acp_enable))
        .route("/api/sessions/{id}/acp/disable", post(api::acp_disable))
        .route(
            "/api/sessions/{id}/queue",
            post(api::queue_enqueue)
                .get(api::queue_list)
                .delete(api::queue_clear),
        )
        .route(
            "/api/sessions/{id}/queue/{promptId}",
            patch(api::queue_edit).delete(api::queue_remove),
        )
        .route(
            "/api/sessions/{id}/acp/approvals/{nonce}",
            post(api::resolve_approval),
        )
        .route(
            "/api/sessions/{id}/acp/elicitations/{nonce}",
            post(api::resolve_elicitation),
        )
        .route("/api/acp/agents", get(api::list_acp_agents))
        .route("/api/acp/option-catalog", get(api::get_option_catalog))
        .route("/api/claude-sessions", get(api::list_claude_sessions));

    // Dashboard bundle (Vite build output) plus the SPA fallback. Without
    // `web` the daemon still answers `/api/*`; browser paths 404.
    #[cfg(feature = "web")]
    let app = app
        .route("/assets/{*path}", get(serve_asset))
        .route("/manifest.json", get(serve_public_file))
        .route("/sw.js", get(serve_public_file))
        .route("/icon-192.png", get(serve_public_file))
        .route("/icon-512.png", get(serve_public_file))
        .fallback(get(serve_index));

    app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        cityhall_gate,
    ))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::auth_middleware,
    ))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        access_policy,
    ))
    .layer(axum::middleware::from_fn(security_headers))
    .layer(axum::middleware::from_fn(http_request_span))
    .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
    .with_state(state)
}

/// Placeholder logged in place of a route template when axum matched no
/// route: the SPA fallback, and the 405 that fallback returns for a
/// non-GET method. A request whose *path* matched a registered route still
/// carries its template even when the method router rejects it, because
/// axum matches on path before it dispatches on method. The raw URI is never
/// substituted here: it is attacker- or user-controlled text, and the whole
/// point of [`log_route`] is that only strings we wrote ourselves reach the
/// log file.
pub(super) const UNMATCHED_ROUTE: &str = "<unmatched>";

/// Route template for a request, for logging only.
///
/// Deliberately never the raw URI. `aoe serve` puts the auth token in the
/// URL's query string, path segments carry session ids, and `debug.log` is a
/// file users paste into issue reports. `MatchedPath` is the template we
/// registered in [`build_router`] (`/api/sessions/{id}/acp/replay`), a
/// compile-time constant with no request data in it, so logging it cannot
/// leak a token or an id no matter what the client sent. Axum fills the
/// extension during routing, before any `Router::layer` middleware runs.
pub(super) fn log_route(request: &axum::extract::Request) -> &str {
    request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or(UNMATCHED_ROUTE, |m| m.as_str())
}

/// Longest client-supplied `X-Request-Id` we are willing to echo. A
/// correlation token needs far less; anything longer is a caller padding
/// every one of its log lines.
pub(super) const MAX_CLIENT_REQUEST_ID: usize = 64;

/// Whether a client-supplied `X-Request-Id` can be reused verbatim.
///
/// Held to the same rule as [`log_route`]. `request_id` is a field of the
/// completion event, so it renders under the default `show_spans = false`
/// formatter, and `HeaderValue::to_str` accepts any visible ASCII, spaces
/// and `=` included. An unfiltered value therefore lets an unauthenticated
/// caller (the event fires outside the auth layer) forge `status=` and
/// `path=` pairs on the very line #3402 added for triage, or pad every 4xx
/// line to churn the file through rotation. Anything outside a bounded
/// token charset is replaced by a generated uuid.
pub(super) fn is_log_safe_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CLIENT_REQUEST_ID
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Middleware that wraps every request in an `http.request` span with a
/// generated or echoed `X-Request-Id`, then emits one completion event at
/// the level matching the response status. Logs fired inside the request
/// (auth middleware, route handlers, downstream `tracing` events) inherit
/// the span fields, so a single grep on `request_id` reconstructs the call.
///
/// The completion event repeats `request_id`, `method`, and `path` as its
/// own fields rather than relying on the span: `[logging].show_spans` is
/// `false` by default, which drops the span prefix from the rendered line
/// and used to leave `completed status=500 latency_ms=0` with nothing to
/// identify the request (#3402). For the same reason the event is emitted
/// outside the span, so enabling `show_spans` prints each field once
/// instead of twice.
///
/// Successful completions (2xx/3xx) emit at `debug`, not `info`: the web
/// UI polls `/api/sessions` every ~2s, so an info-level success log here
/// would flood `debug.log` at the default `info` filter. Users who want
/// to see every request can dial `http.request=debug` from settings;
/// 4xx (`warn`) and 5xx (`error`) stay visible at the default level.
pub(super) async fn http_request_span(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let rid = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| is_log_safe_request_id(v))
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let method = request.method().clone();
    let route = log_route(&request).to_string();
    let span = tracing::debug_span!(
        target: "http.request",
        "http_request",
        request_id = %rid,
        method = %method,
        path = %route,
    );
    let start = std::time::Instant::now();
    let mut response = next.run(request).instrument(span).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    let status = response.status().as_u16();
    // `tracing` resolves the level at compile time, so the branches cannot
    // share one call site; the macro keeps the field list written once.
    macro_rules! completed {
        ($level:ident) => {
            tracing::$level!(
                target: "http.request",
                request_id = %rid,
                method = %method,
                path = %route,
                status,
                latency_ms,
                "completed"
            )
        };
    }
    if status >= 500 {
        completed!(error);
    } else if status >= 400 {
        completed!(warn);
    } else {
        completed!(debug);
    }
    if let Ok(value) = rid.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_helpers::vecs;
    use crate::server::test_support;

    /// `MakeWriter` sink so the request-log test can read back exactly the
    /// bytes a daemon would have appended to `debug.log`.
    #[derive(Clone)]
    struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(src);
            Ok(src.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// #3402: the `http.request` completion line must identify the request.
    /// It is rendered with the default `show_spans = false` formatter, which
    /// drops span fields, so `request_id` / `method` / `path` have to be
    /// fields of the event itself. `path` is the route template, never the
    /// raw URI: `aoe serve` ships its auth token in the query string and
    /// session ids sit in path segments, and neither may reach the log.
    #[tokio::test]
    async fn http_request_log_identifies_request_without_leaking_uri() {
        use tower::ServiceExt;
        const TOKEN: &str = "super-secret-token";
        const SESSION: &str = "sess-9f3a";

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = LogSink(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .event_format(crate::logging::NoSpanFormat)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        // (request URI, expected level, expected `path` field, expected status)
        let cases = [
            (
                format!("/api/sessions/{SESSION}/acp/replay?token={TOKEN}"),
                "ERROR",
                "path=/api/sessions/{id}/acp/replay",
                "status=500",
            ),
            (
                format!("/api/sessions?token={TOKEN}"),
                "DEBUG",
                "path=/api/sessions",
                "status=200",
            ),
            // No route matched, so there is no template to log and the raw
            // URI must not be substituted for one.
            (
                format!("/nope/{SESSION}?token={TOKEN}"),
                "WARN",
                "path=<unmatched>",
                "status=404",
            ),
        ];

        // Drains the sink and returns the one `http.request` completion line,
        // ignoring whatever else the request happened to log.
        let take_line = || {
            let mut sink = buf.lock().unwrap();
            let log = String::from_utf8(sink.clone()).unwrap();
            sink.clear();
            log.lines()
                .find(|l| l.contains("http.request"))
                .unwrap_or_else(|| panic!("no http.request line in {log}"))
                .to_string()
        };

        for (uri, level, path_field, status) in cases {
            let app = axum::Router::new()
                .route(
                    "/api/sessions/{id}/acp/replay",
                    axum::routing::get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
                )
                .route("/api/sessions", axum::routing::get(|| async { "[]" }))
                .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
                .layer(axum::middleware::from_fn(http_request_span));
            let req = axum::http::Request::builder()
                .uri(&uri)
                .body(axum::body::Body::empty())
                .unwrap();
            app.oneshot(req).await.unwrap();

            let line = take_line();
            for expected in [level, path_field, status, "method=GET", "request_id="] {
                assert!(
                    line.contains(expected),
                    "{uri}: want {expected:?}, got {line}"
                );
            }
            for secret in [TOKEN, SESSION] {
                assert!(
                    !line.contains(secret),
                    "{uri}: leaked {secret:?} into {line}"
                );
            }
        }

        // The cases above pin the middleware itself; this pins its position in
        // the real stack, where a template exists only because axum routes the
        // request before any `Router::layer` middleware runs. The token in the
        // query string is wrong, so auth rejects it: exactly the 4xx a triager
        // greps for, and the one request shape that carries a secret.
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["localhost"]),
            vecs(&["http://localhost:8080"]),
            Some("real-token".to_string()),
        );
        let req = axum::http::Request::builder()
            .uri(format!("/api/sessions/{SESSION}/acp/replay?token={TOKEN}"))
            .header("host", "localhost")
            .body(axum::body::Body::empty())
            .unwrap();
        test_support::build_router_for_test(state)
            .oneshot(req)
            .await
            .unwrap();
        let line = take_line();
        assert!(
            line.contains("path=/api/sessions/{id}/acp/replay"),
            "real router: got {line}"
        );
        for secret in [TOKEN, SESSION] {
            assert!(
                !line.contains(secret),
                "real router leaked {secret:?}: {line}"
            );
        }

        // `request_id` is client-supplied, and it lands on the same line as
        // `path`, so it is held to the same rule. `HeaderValue::to_str`
        // admits spaces and `=`, so an unfiltered header forges fields on
        // the line #3402 added for triage. (header value, echoed verbatim?)
        let overlong = "x".repeat(MAX_CLIENT_REQUEST_ID + 1);
        let ids: [(&str, bool); 3] = [
            ("forged status=200 path=/pwned", false),
            (overlong.as_str(), false),
            // A uuid, which is what the dashboard's fetch interceptor sends.
            ("6f1c2b7e-0f2a-4a1e-9d3c-2b8f5a0c7d11", true),
        ];
        for (header, echoed) in ids {
            let app = axum::Router::new()
                .route("/api/sessions", axum::routing::get(|| async { "[]" }))
                .layer(axum::middleware::from_fn(http_request_span));
            let req = axum::http::Request::builder()
                .uri("/api/sessions")
                .header("x-request-id", header)
                .body(axum::body::Body::empty())
                .unwrap();
            app.oneshot(req).await.unwrap();
            let line = take_line();
            assert_eq!(
                line.contains(&format!("request_id={header}")),
                echoed,
                "{header:?}: got {line}"
            );
            assert!(line.contains("request_id="), "no request id at all: {line}");
        }
    }
}
