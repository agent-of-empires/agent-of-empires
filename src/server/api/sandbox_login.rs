//! Sandbox login REST API: the web twin of `aoe sandbox login`. The login
//! helper runs on the host, so starting, answering and cancelling need read-write mode and an
//! elevated session when login is enabled.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::FromRequest;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::plugins::mutation_gate;
use super::{api_error, unknown_profile_response, AppState};
use crate::server::auth::AuthenticatedBrowserSession;
use crate::server::principal::Principal;
use crate::server::sandbox_login::LoginOutcome;
use crate::session::config::container_config::{
    default_login_tool, login_target_with_command, sandbox_login_status, SandboxLoginStatus,
};
use crate::session::sandbox_login::code::LoginCode;
use crate::session::sandbox_login::helper::{LoginError, Retired};

/// A login code is at most 512 bytes; the body limit leaves room for the JSON around it.
pub(crate) const CODE_BODY_LIMIT: usize = 4 * 1024;

#[derive(Deserialize)]
pub struct StartLoginBody {
    #[serde(default)]
    pub force: bool,
    /// The agent whose shared credential file to log in; a wrapper agent with its own
    /// config directory has a file of its own.
    pub tool: Option<String>,
    /// The command the session's launch runs for `tool`, as `aoe sandbox login --command`
    /// takes it: a session with its own command waits on that command's file. Needs `tool`.
    pub command: Option<String>,
    /// The profile of the session, as `aoe -p` takes it: the login lands in that profile's
    /// credential file. Absent or empty is the daemon's profile. Without `tool` the tool is
    /// that profile's default, as `aoe -p <profile> sandbox login` resolves it.
    pub profile: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginStatusQuery {
    pub tool: Option<String>,
    /// As in [`StartLoginBody`].
    pub command: Option<String>,
    /// As in [`StartLoginBody`].
    pub profile: Option<String>,
}

#[derive(Serialize)]
pub struct StartLoginResponse {
    pub login_id: String,
    pub url: String,
    /// Names the dialog that started or joined the login; its cancel passes it back.
    pub lease: String,
}

#[derive(Deserialize)]
pub struct CancelLoginQuery {
    /// The `lease` of the start this cancel is for: the login ends with its last lease.
    /// Without it the cancel ends the login for every dialog.
    pub lease: Option<String>,
}

#[derive(Deserialize)]
pub struct SubmitCodeBody {
    pub code: String,
}

#[derive(Serialize)]
pub struct LoginStatusResponse {
    pub status: SandboxLoginStatus,
    /// Why the login is not applicable, when the runtime cannot run it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub running: Option<RunningLogin>,
    pub held_sessions: usize,
}

#[derive(Serialize)]
pub struct RunningLogin {
    pub login_id: String,
}

/// The `command` of a request, empty counting as none: the launch resolves an empty command
/// as the profile's.
fn requested_command(command: Option<String>) -> Option<String> {
    command.filter(|c| !c.is_empty())
}

/// 400 for a `command` without the `tool` it is for, as `aoe sandbox login --command`
/// requires `--tool`.
fn command_needs_tool(tool: &Option<String>, command: &Option<String>) -> Option<Response> {
    (tool.is_none() && command.is_some()).then(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "sandbox_login_command_needs_tool",
            "name the tool the command is for",
        )
    })
}

/// The profile a request is for: its `profile`, empty counting as none, else the daemon's.
/// 400 for a name that is no profile, so a typo never reads another profile's files nor
/// mints a profile directory; the daemon's own profile needs no lookup. An enumeration
/// failure is a 500.
async fn resolve_profile(
    daemon_profile: &str,
    requested: Option<String>,
) -> Result<String, Response> {
    let Some(name) = requested.filter(|p| !p.is_empty() && p != daemon_profile) else {
        return Ok(daemon_profile.to_string());
    };
    let unknown = || {
        api_error(
            StatusCode::BAD_REQUEST,
            "sandbox_login_unknown_profile",
            "name an existing profile",
        )
    };
    if super::validate_profile_name(&name).is_err() {
        return Err(unknown());
    }
    match unknown_profile_response(&name) {
        None => Ok(name),
        Some(refused) if refused.status() == StatusCode::BAD_REQUEST => Err(unknown()),
        Some(failure) => Err(failure),
    }
}

/// What a request's `tool` and `command` mean for `profile`: the tool it names,
/// else the profile's default login tool, and the credential file the sandboxes of that
/// tool and command wait on. `None` when no tool is named or implied, or the tool has no
/// sandbox login.
fn login_target_of(
    profile: &str,
    named: Option<String>,
    command: Option<&str>,
) -> Option<(String, PathBuf)> {
    let tool = named.or_else(|| default_login_tool(profile))?;
    let credential = login_target_with_command(profile, &tool, command)?.credential;
    Some((tool, credential))
}

/// Whether `separate_login` is on while the active container runtime cannot run the login
/// helper, so a login can never start. Under `host_seed` the setting is what refuses.
fn runtime_cannot_log_in(profile: &str) -> bool {
    crate::session::config::profile_config::resolve_config_or_warn(profile)
        .sandbox
        .shared_credential
        == crate::session::config::SharedCredentialMode::SeparateLogin
        && !crate::session::sandbox_login::helper::supports_runtime(
            crate::containers::get_container_runtime().kind,
        )
}

/// Why a start must not reach the helper on a runtime that cannot run it, as the launch gate
/// and `sandbox_login_status` read it: the runtime only matters when a login is wanted. A
/// usable credential needs none (`AlreadyLoggedIn`) unless `force` asks for one; otherwise
/// the runtime refuses it.
fn start_refusal(
    profile: &str,
    tool: &str,
    command: Option<&str>,
    force: bool,
) -> Option<LoginError> {
    if !runtime_cannot_log_in(profile) {
        return None;
    }
    Some(match sandbox_login_status(profile, tool, command) {
        SandboxLoginStatus::Ok { .. } if !force => LoginError::AlreadyLoggedIn,
        _ => LoginError::Unsupported(crate::session::SandboxLoginUnsupported),
    })
}

/// `GET /api/sandbox/login?tool=&command=&profile=`: whether the sandboxes of `profile` (default:
/// the daemon's) of `tool` (default: the profile's default login tool) and `command` need the
/// login, the login in progress for their credential file and how many sessions wait on it.
/// Never the sign-in link.
pub async fn get_sandbox_login(
    State(state): State<Arc<AppState>>,
    Query(LoginStatusQuery {
        tool,
        command,
        profile,
    }): Query<LoginStatusQuery>,
) -> Response {
    let command = requested_command(command);
    if let Some(refused) = command_needs_tool(&tool, &command) {
        return refused;
    }
    let daemon_profile = state.served_profile().clone();
    let profile = match resolve_profile(&daemon_profile, profile).await {
        Ok(profile) => profile,
        Err(refused) => return refused,
    };
    let read = tokio::task::spawn_blocking(move || {
        login_target_of(&profile, tool, command.as_deref()).map(|(tool, credential)| {
            (
                sandbox_login_status(&profile, &tool, command.as_deref()),
                credential,
            )
        })
    })
    .await;
    let target = match read {
        Ok(target) => target,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let login = &state.session_service.sandbox_login;
    let Some((status, credential)) = target else {
        return Json(LoginStatusResponse {
            status: SandboxLoginStatus::NotApplicable,
            reason: None,
            running: None,
            held_sessions: 0,
        })
        .into_response();
    };
    // A runtime that cannot run the helper has no login to offer: not applicable, with why.
    let (status, reason) = match status {
        SandboxLoginStatus::Unsupported => (
            SandboxLoginStatus::NotApplicable,
            Some(crate::session::SandboxLoginUnsupported.to_string()),
        ),
        other => (other, None),
    };
    Json(LoginStatusResponse {
        status,
        reason,
        running: login.running_on(&credential).map(|helper| RunningLogin {
            login_id: helper.id().to_string(),
        }),
        held_sessions: login.held_for(&credential),
    })
    .into_response()
}

/// `POST /api/sandbox/login`: start a login, or return the running one. The optional
/// `{"force": bool, "tool": string, "command": string, "profile": string}` body is read only after the gate, so an unelevated
/// caller gets the 403.
pub async fn start_sandbox_login(
    State(state): State<Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let body = match Option::<Json<StartLoginBody>>::from_request(request, &()).await {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let (force, tool, command, profile) = body.map_or((false, None, None, None), |Json(body)| {
        (
            body.force,
            body.tool,
            requested_command(body.command),
            body.profile,
        )
    });
    if let Some(refused) = command_needs_tool(&tool, &command) {
        return refused;
    }
    let daemon_profile = state.served_profile().clone();
    let profile = match resolve_profile(&daemon_profile, profile).await {
        Ok(profile) => profile,
        Err(refused) => return refused,
    };
    let (owned, requested) = (profile.clone(), command.clone());
    let target = tokio::task::spawn_blocking(move || {
        login_target_of(&owned, tool, requested.as_deref()).map(|(tool, _)| {
            let refusal = start_refusal(&owned, &tool, requested.as_deref(), force);
            (tool, refusal)
        })
    })
    .await;
    let target = match target {
        Ok(target) => target,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    let Some((tool, refusal)) = target else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "sandbox_login_tool_unsupported",
            "name a tool with a sandbox login to log in",
        );
    };
    // Before any helper, so nothing is created or started for a login that cannot happen.
    if let Some(refusal) = refusal {
        return login_error_response(refusal);
    }
    match state
        .session_service
        .sandbox_login
        .start_login(&profile, &tool, command.as_deref(), force)
        .await
    {
        Ok((helper, lease)) => Json(StartLoginResponse {
            login_id: helper.id().to_string(),
            url: helper.url().to_string(),
            lease,
        })
        .into_response(),
        Err(error) => login_error_response(error),
    }
}

/// `POST /api/sandbox/login/{login_id}/code`: hand the code to the login. A malformed
/// code is refused before the login client sees it and the login keeps running; every other outcome
/// ends the helper, so a new link needs a new login.
pub async fn submit_sandbox_login_code(
    State(state): State<Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(login_id): Path<String>,
    body: Result<Json<SubmitCodeBody>, JsonRejection>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let Json(SubmitCodeBody { code }) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let login = &state.session_service.sandbox_login;
    let Some(helper) = login.running_login(&login_id) else {
        return login_error_response(LoginError::Gone);
    };
    let code = match LoginCode::parse(code) {
        Ok(code) => code,
        Err(invalid) => {
            return api_error(StatusCode::BAD_REQUEST, "invalid_code", invalid.to_string())
        }
    };
    // The settle steps must run even when the client disconnects mid-submit: after a
    // rejected code the login client never exits, so an abandoned request would leave the helper
    // registered and holding the login lock until its total timeout.
    let settle = tokio::spawn(async move {
        let login = &state.session_service.sandbox_login;
        let outcome = helper.submit_code(code).await;
        // The slot stays taken until the helper let go of the credential, so no second helper
        // starts meanwhile; a helper still ending stays for the next cancel or start to retry.
        if helper.cancel().await != Retired::Pending {
            login.forget_login(&login_id);
        }
        match outcome {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(error) => login_error_response(error),
        }
    });
    settle.await.unwrap_or_else(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the login task failed",
        )
    })
}

/// `GET /api/sandbox/login/{login_id}`: what the code exchange of the login came to, for a
/// client that lost the answer to its code: `{"outcome": "succeeded" | "failed" | "pending" |
/// "unknown"}`. Needs no elevation; the id is the only secret it takes.
pub async fn get_sandbox_login_outcome(
    State(state): State<Arc<AppState>>,
    Path(login_id): Path<String>,
) -> Response {
    let outcome = match state
        .session_service
        .sandbox_login
        .outcome_of(&login_id)
        .await
    {
        LoginOutcome::Succeeded => "succeeded",
        LoginOutcome::Failed => "failed",
        LoginOutcome::Pending => "pending",
        LoginOutcome::Unknown => "unknown",
    };
    Json(json!({ "outcome": outcome })).into_response()
}

/// `DELETE /api/sandbox/login/{login_id}?lease=`: a dialog leaves the login. Dialogs for one
/// credential share a login, so it ends with the last lease, or at once without `lease`.
/// Idempotent, so a login that already ended answers the same. A code exchange in progress
/// is left to settle.
pub async fn cancel_sandbox_login(
    State(state): State<Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedBrowserSession>>,
    loopback: Option<axum::Extension<Principal>>,
    Path(login_id): Path<String>,
    Query(CancelLoginQuery { lease }): Query<CancelLoginQuery>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref(), loopback.as_deref()).await {
        return resp;
    }
    let login = &state.session_service.sandbox_login;
    if let Some(helper) = login.running_login(&login_id) {
        // A helper still ending stays in its slot, owning the credential lock.
        if helper
            .release(lease.as_deref())
            .await
            .is_some_and(|retired| retired != Retired::Pending)
        {
            login.forget_login(&login_id);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The error text is already free of the code, the link and its `state`: the helper builds
/// it from redacted output.
fn login_error_response(error: LoginError) -> Response {
    let (status, code, message) = match error {
        LoginError::Busy => (
            StatusCode::CONFLICT,
            "sandbox_login_busy",
            error.to_string(),
        ),
        LoginError::NotSeparateLogin => (
            StatusCode::CONFLICT,
            "sandbox_login_not_separate_login",
            error.to_string(),
        ),
        LoginError::AlreadyLoggedIn => (
            StatusCode::CONFLICT,
            "sandbox_login_already_ok",
            "the sandbox credential is already usable; send force to log in again".to_string(),
        ),
        LoginError::Unsupported(_) => (
            StatusCode::CONFLICT,
            crate::session::SandboxLoginUnsupported::CODE,
            error.to_string(),
        ),
        LoginError::NoUrl { output } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "sandbox_login_no_url",
            output,
        ),
        LoginError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "sandbox_login_timeout",
            error.to_string(),
        ),
        LoginError::Cancelled => (
            StatusCode::CONFLICT,
            "sandbox_login_cancelled",
            error.to_string(),
        ),
        LoginError::Failed { message } => {
            (StatusCode::UNPROCESSABLE_ENTITY, "login_failed", message)
        }
        LoginError::Gone => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "login_not_found" })),
            )
                .into_response()
        }
        LoginError::Other(_) => {
            tracing::error!(target: "http.api.sandbox_login", "sandbox login helper failed: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "the sandbox login helper failed; see the daemon log".to_string(),
            )
        }
    };
    api_error(status, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_helpers::vecs;
    use crate::session::sandbox_login::code::CodeError;
    use crate::session::sandbox_login::helper::HelperTimeouts;
    use crate::session::sandbox_login::test_fake::{Fake, MEASURED_URL};
    use axum::http::Method;
    use std::time::Duration;
    use tower::ServiceExt;

    const TOKEN: &str = "secret-token";

    /// Fields drop in order: the PATH guard before the app dir guard that owns the env lock.
    struct Env {
        _runtime: crate::session::test_support::EnvGuard,
        _home: crate::session::test_support::AppDirGuard,
        state: Arc<AppState>,
        app: axum::Router,
        fake: Arc<Fake>,
        held_id: String,
    }

    fn env() -> Env {
        env_with(HelperTimeouts::default())
    }

    /// A daemon whose only session waits for the login, and a fake helper behind the
    /// service in place of a container.
    fn env_with(timeouts: HelperTimeouts) -> Env {
        let home = crate::session::test_support::isolate_app_dir();
        let (runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-login-api");
        inst.view = crate::session::View::Structured;
        let shared = crate::session::test_support::sandbox_needing_login(home.path(), &mut inst);
        let held_id = inst.id.clone();
        let blocked = inst.ensure_sandbox_login().unwrap_err();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            vec![inst],
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        let login = &state.session_service.sandbox_login;
        login.hold(
            &held_id,
            crate::server::sandbox_login::LaunchOrigin::Interactive,
            &blocked,
        );
        let fake = Arc::new(Fake::at(tempfile::TempDir::new().unwrap(), shared));
        let starter = Arc::clone(&fake);
        login.set_test_starter(move |_profile, _tool, _command| {
            let fake = Arc::clone(&starter);
            Box::pin(async move { fake.start_timed(true, timeouts).await })
        });
        let app = crate::server::test_support::build_router_for_test(Arc::clone(&state));
        Env {
            _runtime: runtime,
            _home: home,
            state,
            app,
            fake,
            held_id,
        }
    }

    async fn call(
        app: &axum::Router,
        method: Method,
        uri: &str,
        body: Option<String>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1")
            .header("cookie", format!("aoe_token={TOKEN}"));
        let body = match body {
            Some(body) => {
                req = req.header("content-type", "application/json");
                axum::body::Body::from(body)
            }
            None => axum::body::Body::empty(),
        };
        let mut req = req.body(body).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                5555,
            ))));
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    const BASE: &str = "/api/sandbox/login";

    async fn start(env: &Env) -> (StatusCode, serde_json::Value) {
        call(&env.app, Method::POST, BASE, Some("{}".to_string())).await
    }

    async fn send_code(env: &Env, login_id: &str, code: &str) -> (StatusCode, serde_json::Value) {
        call(
            &env.app,
            Method::POST,
            &format!("{BASE}/{login_id}/code"),
            Some(json!({ "code": code }).to_string()),
        )
        .await
    }

    fn empty_request() -> axum::extract::Request {
        axum::http::Request::post("/")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_requires_elevation() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.login_manager = Arc::new(crate::server::login::LoginManager::new(Some("pw")));
            });
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let unelevated = || {
            Some(axum::Extension(AuthenticatedBrowserSession(
                session.clone(),
            )))
        };
        let body = || {
            Ok(Json(SubmitCodeBody {
                code: "c".to_string(),
            }))
        };

        let refused = [
            start_sandbox_login(State(state.clone()), unelevated(), None, empty_request()).await,
            submit_sandbox_login_code(
                State(state.clone()),
                unelevated(),
                None,
                Path("0123456789abcdef".to_string()),
                body(),
            )
            .await,
            cancel_sandbox_login(
                State(state.clone()),
                unelevated(),
                None,
                Path("0123456789abcdef".to_string()),
                Query(CancelLoginQuery { lease: None }),
            )
            .await,
        ];
        for resp in refused {
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"], "elevation_required");
        }
        assert_eq!(
            get_sandbox_login(
                State(state.clone()),
                Query(LoginStatusQuery {
                    tool: None,
                    command: None,
                    profile: None,
                })
            )
            .await
            .status(),
            StatusCode::OK,
            "reading the status needs no elevation"
        );

        state.login_manager.elevate_session(&session).await;
        assert_eq!(
            cancel_sandbox_login(
                State(state.clone()),
                unelevated(),
                None,
                Path("0123456789abcdef".to_string()),
                Query(CancelLoginQuery { lease: None }),
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );

        let read_only =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.read_only = true;
            });
        let resp = start_sandbox_login(State(read_only), None, None, empty_request()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_start_returns_running_helper() {
        let env = env();
        let (status, first) = start(&env).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["url"], MEASURED_URL);
        let (_, second) = start(&env).await;
        assert_eq!(
            first["login_id"], second["login_id"],
            "one helper at a time"
        );

        let (status, view) = call(&env.app, Method::GET, BASE, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["status"]["state"], "needed");
        assert_eq!(view["running"]["login_id"], first["login_id"]);
        assert_eq!(view["held_sessions"], 1);
        assert!(
            !view.to_string().contains("SAMPLEstate"),
            "the status never carries the link"
        );

        let (status, _) = call(
            &env.app,
            Method::DELETE,
            &format!("{BASE}/{}", first["login_id"].as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(
            env.fake.cleaned().exists(),
            "the helper was killed and cleaned up"
        );
        let (_, view) = call(&env.app, Method::GET, BASE, None).await;
        assert!(view["running"].is_null());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_code_paths() {
        let env = env();
        let (_, first) = start(&env).await;
        let first_id = first["login_id"].as_str().unwrap().to_string();

        let (status, body) = send_code(&env, &first_id, "has space").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_code");
        assert_eq!(body["message"], CodeError::InvalidCharacter.to_string());
        assert!(
            env.state
                .session_service
                .sandbox_login
                .running_login(&first_id)
                .is_some(),
            "a malformed code leaves the login running"
        );

        let (status, body) = send_code(&env, &first_id, "wrong-code").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "login_failed");
        assert!(body["message"].as_str().unwrap().contains("Login failed"));
        let (status, body) = send_code(&env, &first_id, "good-code#good-state").await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a wrong code ends the helper"
        );
        assert_eq!(body["error"], "login_not_found");

        let (_, second) = start(&env).await;
        assert_ne!(
            second["login_id"], first["login_id"],
            "a new link needs a new login"
        );
        let login = &env.state.session_service.sandbox_login;
        assert!(login.is_held(&env.held_id));
        let (status, body) = send_code(
            &env,
            second["login_id"].as_str().unwrap(),
            "good-code#good-state",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));
        assert!(
            login.is_held(&env.held_id),
            "the reconciler tick owns the release, with its park bookkeeping"
        );
        let (_, view) = call(&env.app, Method::GET, BASE, None).await;
        assert_eq!(view["status"]["state"], "ok");
        assert!(view["running"].is_null());

        reconcile_once(&env.state).await;
        assert!(
            !login.is_held(&env.held_id),
            "the next tick releases the session"
        );
        let (_, view) = call(&env.app, Method::GET, BASE, None).await;
        assert_eq!(view["held_sessions"], 0);
    }

    /// One reconciler tick with the cadence-gated passes sitting out.
    async fn reconcile_once(state: &Arc<AppState>) {
        let now = Some(std::time::Instant::now());
        let mut cadence = crate::server::acp_reconciler::ReapCadence {
            idle: now,
            rate_limit: now,
            terminal_repair: now,
        };
        crate::server::acp_reconciler::reconcile_acp_workers(
            state,
            &mut Default::default(),
            &mut cadence,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        )
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_submit_survives_a_dropped_request() {
        let env = env_with(HelperTimeouts {
            after_code: Duration::from_millis(400),
            ..HelperTimeouts::default()
        });
        let (_, started) = start(&env).await;
        let login_id = started["login_id"].as_str().unwrap().to_string();
        let login = &env.state.session_service.sandbox_login;
        let helper = login.running_login(&login_id).expect("running");

        // The fake never answers a `hang` code, so the request is mid-submit when it drops.
        let request = submit_sandbox_login_code(
            State(Arc::clone(&env.state)),
            None,
            None,
            Path(login_id.clone()),
            Ok(Json(SubmitCodeBody {
                code: "hang-forever".to_string(),
            })),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), request)
                .await
                .is_err(),
            "the request is still waiting for the login client when the HTTP client disconnects"
        );

        tokio::time::timeout(Duration::from_secs(10), async {
            while !(helper.is_finished() && login.running_login(&login_id).is_none()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the abandoned submit still ends the helper and frees the slot");
        assert!(env.fake.cleaned().exists(), "its container cleanup ran");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_a_cancel_never_ends_a_code_exchange_in_progress() {
        let env = env_with(HelperTimeouts {
            after_code: Duration::from_secs(2),
            ..HelperTimeouts::default()
        });
        let (_, first) = start(&env).await;
        // A second dialog for the same credential gets the same running login.
        let (_, second) = start(&env).await;
        let login_id = first["login_id"].as_str().unwrap().to_string();
        assert_eq!(second["login_id"], first["login_id"]);
        let login = &env.state.session_service.sandbox_login;
        let helper = login.running_login(&login_id).expect("running");

        // The fake never answers a `hang` code, so the exchange is in progress until its timeout.
        let submit = tokio::spawn(submit_sandbox_login_code(
            State(Arc::clone(&env.state)),
            None,
            None,
            Path(login_id.clone()),
            Ok(Json(SubmitCodeBody {
                code: "hang-forever".to_string(),
            })),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !helper.is_submitting() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the exchange starts");

        // The idle dialog closes while the other one exchanges its code.
        let (status, _) = call(
            &env.app,
            Method::DELETE,
            &format!("{BASE}/{login_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!helper.is_finished(), "the exchange was not cancelled");
        assert!(login.running_login(&login_id).is_some());

        submit.await.expect("the exchange settles on its own");
        assert!(
            login.running_login(&login_id).is_none(),
            "and frees the slot"
        );
    }

    fn lease_of(started: &serde_json::Value) -> String {
        started["lease"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_a_dialog_leaving_keeps_the_login_for_the_others() {
        let env = env();
        let (_, first) = start(&env).await;
        let (_, second) = start(&env).await;
        let login_id = first["login_id"].as_str().unwrap().to_string();
        assert_eq!(second["login_id"], first["login_id"]);
        assert_ne!(lease_of(&first), lease_of(&second), "one lease per dialog");
        let login = &env.state.session_service.sandbox_login;
        let leave = |lease: String| {
            let uri = format!("{BASE}/{login_id}?lease={lease}");
            let app = env.app.clone();
            async move { call(&app, Method::DELETE, &uri, None).await.0 }
        };

        assert_eq!(leave(lease_of(&first)).await, StatusCode::NO_CONTENT);
        assert!(
            login.running_login(&login_id).is_some() && !env.fake.cleaned().exists(),
            "the other dialog still has its login"
        );
        assert_eq!(leave(lease_of(&first)).await, StatusCode::NO_CONTENT);
        assert!(
            login.running_login(&login_id).is_some(),
            "a repeated leave does not count for the other dialog"
        );

        assert_eq!(leave(lease_of(&second)).await, StatusCode::NO_CONTENT);
        assert!(login.running_login(&login_id).is_none());
        assert!(env.fake.cleaned().exists(), "the last leave ends it");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_a_login_taken_down_is_never_joined() {
        let env = env();
        let (_, first) = start(&env).await;
        let login_id = first["login_id"].as_str().unwrap().to_string();
        let login = &env.state.session_service.sandbox_login;
        let helper = login.running_login(&login_id).expect("running");

        // The cancel is done, but its handler has not forgotten the login yet.
        assert!(helper.release(Some(&lease_of(&first))).await.is_some());
        assert!(helper.join().is_none(), "a cancelled login takes no lease");

        let (status, second) = start(&env).await;
        assert_eq!(status, StatusCode::OK);
        assert_ne!(second["login_id"], first["login_id"], "a fresh login");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_reports_what_the_code_exchange_came_to() {
        let env = env();
        let outcome = |login_id: String| {
            let app = env.app.clone();
            async move {
                let (status, body) =
                    call(&app, Method::GET, &format!("{BASE}/{login_id}"), None).await;
                assert_eq!(status, StatusCode::OK);
                body["outcome"].as_str().unwrap().to_string()
            }
        };
        assert_eq!(outcome("0000000000000000".to_string()).await, "unknown");

        let (_, first) = start(&env).await;
        let first_id = first["login_id"].as_str().unwrap().to_string();
        assert_eq!(outcome(first_id.clone()).await, "pending");
        let (status, _) = send_code(&env, &first_id, "wrong-code").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(outcome(first_id.clone()).await, "failed");

        let (_, second) = start(&env).await;
        let second_id = second["login_id"].as_str().unwrap().to_string();
        let (status, _) = send_code(&env, &second_id, "good-code#good-state").await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send_code(&env, &second_id, "good-code#good-state").await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("login_not_found")),
            "a retry after a lost answer finds the login gone"
        );
        assert_eq!(
            outcome(second_id).await,
            "succeeded",
            "and still learns the exchange worked"
        );
        assert_eq!(outcome(first_id).await, "failed", "an older one too");
    }

    /// Starts a hanging exchange on a login two dialogs share and waits until it runs.
    async fn exchange_in_progress(env: &Env, login_id: &str) -> tokio::task::JoinHandle<Response> {
        let helper = env
            .state
            .session_service
            .sandbox_login
            .running_login(login_id)
            .expect("running");
        let submit = tokio::spawn(submit_sandbox_login_code(
            State(Arc::clone(&env.state)),
            None,
            None,
            Path(login_id.to_string()),
            Ok(Json(SubmitCodeBody {
                code: "hang-forever".to_string(),
            })),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !helper.is_submitting() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the exchange starts");
        submit
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_the_last_lease_leaving_mid_exchange_leaves_it_to_settle() {
        let env = env_with(HelperTimeouts {
            after_code: Duration::from_millis(600),
            ..HelperTimeouts::default()
        });
        let (_, first) = start(&env).await;
        let login_id = first["login_id"].as_str().unwrap().to_string();
        let submit = exchange_in_progress(&env, &login_id).await;

        let uri = format!("{BASE}/{login_id}?lease={}", lease_of(&first));
        let (status, _) = call(&env.app, Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let login = &env.state.session_service.sandbox_login;
        assert!(
            login.running_login(&login_id).is_some() && !env.fake.cleaned().exists(),
            "no lease is left, but the exchange is not cancelled"
        );
        submit.await.expect("the exchange settles on its own");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_a_start_during_an_exchange_waits_and_gets_a_fresh_login() {
        let env = env_with(HelperTimeouts {
            after_code: Duration::from_millis(600),
            ..HelperTimeouts::default()
        });
        let (_, first) = start(&env).await;
        let login_id = first["login_id"].as_str().unwrap().to_string();
        let submit = exchange_in_progress(&env, &login_id).await;

        let (status, second) = start(&env).await;
        assert_eq!(status, StatusCode::OK);
        assert_ne!(
            second["login_id"], first["login_id"],
            "the login being exchanged is not handed out again"
        );
        submit.await.expect("the exchange settled on its own");
        let (status, body) = call(&env.app, Method::GET, &format!("{BASE}/{login_id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["outcome"], "failed", "its outcome is kept");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_the_outcome_read_waits_for_an_exchange_in_progress() {
        let env = env_with(HelperTimeouts {
            after_code: Duration::from_millis(600),
            ..HelperTimeouts::default()
        });
        let (_, first) = start(&env).await;
        let login_id = first["login_id"].as_str().unwrap().to_string();
        let submit = exchange_in_progress(&env, &login_id).await;

        // The exchange times out without a credential: the read answers with that, not `pending`.
        let (status, body) = call(&env.app, Method::GET, &format!("{BASE}/{login_id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["outcome"], "failed");
        submit.await.expect("the exchange settles");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_other_login_ids_never_touch_the_running_login() {
        let env = env();
        let (_, started) = start(&env).await;
        let login_id = started["login_id"].as_str().unwrap();
        let login = &env.state.session_service.sandbox_login;

        let (status, body) = send_code(&env, "0000000000000000", "good-code#good-state").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "login_not_found");
        let (status, _) = call(
            &env.app,
            Method::DELETE,
            &format!("{BASE}/0000000000000000"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let helper = login.running_login(login_id).expect("still running");
        assert!(!helper.is_finished());
        assert!(!env.fake.cleaned().exists(), "nothing was cancelled");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_start_gate_runs_before_the_body() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state =
            crate::server::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.login_manager = Arc::new(crate::server::login::LoginManager::new(Some("pw")));
            });
        let session = state
            .login_manager
            .create_session(b"bind", "127.0.0.1", "t")
            .await;
        let app = axum::Router::new()
            .route("/login", axum::routing::post(start_sandbox_login))
            .layer(axum::Extension(AuthenticatedBrowserSession(session)))
            .with_state(state);
        for (content_type, body) in [
            ("application/json", "{not json"),
            ("text/plain", "force"),
            ("application/json", r#"{"force":"x"}"#),
        ] {
            let req = axum::http::Request::post("/login")
                .header("content-type", content_type)
                .body(axum::body::Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{content_type} {body}: an unelevated caller never reaches the body"
            );
        }
    }

    #[tokio::test]
    async fn sandbox_login_api_internal_errors_carry_a_fixed_message() {
        let resp = login_error_response(LoginError::Other(
            anyhow::anyhow!("secret detail").context("opening /home/someone/.claude/lock"),
        ));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            !text.contains("/home/someone") && !text.contains("secret detail"),
            "{text}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_body_limits_go_through_the_router() {
        let env = env();
        let (status, _) = call(
            &env.app,
            Method::POST,
            BASE,
            Some(json!({ "force": false, "pad": "a".repeat(5 * 1024) }).to_string()),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "start takes a tiny body"
        );

        let (_, started) = start(&env).await;
        let login_id = started["login_id"].as_str().unwrap();
        let (status, body) = send_code(&env, login_id, &"a".repeat(3 * 1024)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a body under 4 KiB reaches the handler, which refuses the 3 KiB code"
        );
        assert_eq!(body["error"], "invalid_code");
        assert_eq!(body["message"], CodeError::TooLong.to_string());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_body_too_large() {
        let env = env();
        let (_, started) = start(&env).await;
        let login_id = started["login_id"].as_str().unwrap();
        let (status, _) = send_code(&env, login_id, &"a".repeat(5 * 1024)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            env.state
                .session_service
                .sandbox_login
                .running_login(login_id)
                .is_some(),
            "the oversized body never reached the login"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_logs_the_tool_the_request_names_in() {
        use std::sync::Mutex;

        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let mut plain = crate::session::Instance::new("plain", "/tmp/aoe-login-api-plain");
        plain.view = crate::session::View::Structured;
        let plain_file =
            crate::session::test_support::sandbox_needing_login(home.path(), &mut plain);
        crate::session::config::update_config(|c| {
            c.session
                .custom_agents
                .insert("claude-personal".to_string(), "claude".to_string());
            c.session
                .agent_detect_as
                .insert("claude-personal".to_string(), "claude".to_string());
            c.session.agent_config_dir.insert(
                "claude-personal".to_string(),
                "~/.claude-personal".to_string(),
            );
        })
        .unwrap();
        let wrapper_file = home
            .path()
            .join(".claude-personal/sandbox-v2/.credentials.json");
        let mut wrapper = crate::session::Instance::new("wrapper", "/tmp/aoe-login-api-wrapper");
        wrapper.view = crate::session::View::Structured;
        wrapper.tool = "claude-personal".to_string();
        wrapper.command = "claude".to_string();
        wrapper.sandbox_info = plain.sandbox_info.clone();
        let (plain_id, wrapper_id) = (plain.id.clone(), wrapper.id.clone());
        let holds = [
            (plain_id.clone(), plain.ensure_sandbox_login().unwrap_err()),
            (
                wrapper_id.clone(),
                wrapper.ensure_sandbox_login().unwrap_err(),
            ),
        ];
        let state = crate::server::test_support::build_test_app_state_with_policy(
            vec![plain, wrapper],
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        let login = &state.session_service.sandbox_login;
        for (id, blocked) in &holds {
            login.hold(
                id,
                crate::server::sandbox_login::LaunchOrigin::Interactive,
                blocked,
            );
        }
        let fakes: std::collections::HashMap<String, Arc<Fake>> = [
            ("claude", plain_file),
            ("claude-personal", wrapper_file.clone()),
        ]
        .into_iter()
        .map(|(tool, file)| {
            (
                tool.to_string(),
                Arc::new(Fake::at(tempfile::TempDir::new().unwrap(), file)),
            )
        })
        .collect();
        let started_for = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = Arc::clone(&started_for);
        login.set_test_starter(move |_profile, tool, _command| {
            recorded.lock().unwrap().push(tool.to_string());
            let fake = Arc::clone(&fakes[tool]);
            Box::pin(async move { fake.start_timed(true, HelperTimeouts::default()).await })
        });
        let app = crate::server::test_support::build_router_for_test(Arc::clone(&state));
        let wrapper_uri = format!("{BASE}?tool=claude-personal");

        // Status and held sessions are per tool: each session waits on its own file.
        let (_, view) = call(&app, Method::GET, BASE, None).await;
        assert_eq!(view["status"]["state"], "needed");
        assert_eq!(view["held_sessions"], 1, "only the plain session");
        let (_, view) = call(&app, Method::GET, &wrapper_uri, None).await;
        assert_eq!(view["status"]["state"], "needed");
        assert_eq!(view["held_sessions"], 1, "only the wrapper session");

        // A start names its tool; the login runs for that file only.
        let (status, started) = call(
            &app,
            Method::POST,
            BASE,
            Some(json!({ "tool": "claude-personal" }).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(*started_for.lock().unwrap(), ["claude-personal"]);
        let login_id = started["login_id"].as_str().unwrap().to_string();
        let (_, view) = call(&app, Method::GET, &wrapper_uri, None).await;
        assert_eq!(view["running"]["login_id"], login_id.as_str());
        let (_, view) = call(&app, Method::GET, BASE, None).await;
        assert!(view["running"].is_null(), "not the plain file's login");
        let (status, body) = call(&app, Method::POST, BASE, Some("{}".to_string())).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::CONFLICT, Some("sandbox_login_busy")),
            "the one running login is another file's, so it is not handed out"
        );
        let (_, again) = call(
            &app,
            Method::POST,
            BASE,
            Some(json!({ "tool": "claude-personal" }).to_string()),
        )
        .await;
        assert_eq!(
            again["login_id"],
            login_id.as_str(),
            "the same file's login"
        );

        let (status, _) = call(
            &app,
            Method::POST,
            &format!("{BASE}/{login_id}/code"),
            Some(json!({ "code": "good-code#good-state" }).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, view) = call(&app, Method::GET, &wrapper_uri, None).await;
        assert_eq!(view["status"]["state"], "ok");
        let (_, view) = call(&app, Method::GET, BASE, None).await;
        assert_eq!(
            view["status"]["state"], "needed",
            "plain claude still waits"
        );

        reconcile_once(&state).await;
        assert!(!login.is_held(&wrapper_id), "the wrapper session resumes");
        assert!(login.is_held(&plain_id), "the plain session keeps waiting");
        let (_, view) = call(&app, Method::GET, BASE, None).await;
        assert_eq!(view["held_sessions"], 1);
        let (_, view) = call(&app, Method::GET, &wrapper_uri, None).await;
        assert_eq!(view["held_sessions"], 0);

        // A tool with no shared credential file is refused, not a server error.
        let (status, body) = call(
            &app,
            Method::POST,
            BASE,
            Some(json!({ "tool": "codex" }).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "sandbox_login_tool_unsupported");
        assert_eq!(*started_for.lock().unwrap(), ["claude-personal"]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_command_override_tool_logs_in_the_file_the_gate_reads() {
        // `personal` is a codex agent whose launch command is overridden to claude, with its
        // own config directory: it launches claude, so its file is the one the gate reads.
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential =
                crate::session::config::SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_command_override
                .insert("personal".to_string(), "claude".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.claude-personal".to_string());
        })
        .unwrap();
        let target = home
            .path()
            .join(".claude-personal/sandbox-v2/.credentials.json");
        let mut session = crate::session::Instance::new("personal", "/tmp/aoe-login-api-personal");
        session.view = crate::session::View::Structured;
        session.tool = "personal".to_string();
        session.command = "claude".to_string();
        session.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-login-api-personal".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        let blocked = session.ensure_sandbox_login().unwrap_err();
        assert_eq!(blocked.credential, target, "the gate waits on this file");
        let session_id = session.id.clone();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            vec![session],
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        let login = &state.session_service.sandbox_login;
        login.hold(
            &session_id,
            crate::server::sandbox_login::LaunchOrigin::Interactive,
            &blocked,
        );
        let fake = Arc::new(Fake::at(tempfile::TempDir::new().unwrap(), target.clone()));
        login.set_test_starter(move |_profile, _tool, _command| {
            let fake = Arc::clone(&fake);
            Box::pin(async move { fake.start_timed(true, HelperTimeouts::default()).await })
        });
        let app = crate::server::test_support::build_router_for_test(Arc::clone(&state));
        let uri = format!("{BASE}?tool=personal");

        let (_, view) = call(&app, Method::GET, &uri, None).await;
        assert_eq!(view["status"]["state"], "needed", "not `not_applicable`");
        assert_eq!(view["held_sessions"], 1);

        let body = json!({ "tool": "personal" }).to_string();
        let (status, started) = call(&app, Method::POST, BASE, Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let login_id = started["login_id"].as_str().unwrap();
        let (_, view) = call(&app, Method::GET, &uri, None).await;
        assert_eq!(view["running"]["login_id"], login_id);

        let (status, _) = call(
            &app,
            Method::POST,
            &format!("{BASE}/{login_id}/code"),
            Some(json!({ "code": "good-code#good-state" }).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, view) = call(&app, Method::GET, &uri, None).await;
        assert_eq!(view["status"]["state"], "ok");
        assert!(
            login.is_held(&session_id),
            "the reconciler tick owns the release"
        );
        reconcile_once(&state).await;
        assert!(!login.is_held(&session_id), "the login released it");
        let (_, view) = call(&app, Method::GET, &uri, None).await;
        assert_eq!(view["held_sessions"], 0);
    }

    /// A session created with its own command for its tool waits on the file of that command,
    /// which the profile's command for the tool does not reach: the status and the start take
    /// the same `command` as `aoe sandbox login --command` and read and log in that file.
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_command_selects_the_file_a_session_command_waits_on() {
        use std::sync::Mutex;

        // `personal` is a codex agent with its own config directory; only a session whose own
        // command is claude launches claude, so only it waits on a claude login.
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential =
                crate::session::config::SharedCredentialMode::SeparateLogin;
            c.session
                .custom_agents
                .insert("personal".to_string(), "codex".to_string());
            c.session
                .agent_config_dir
                .insert("personal".to_string(), "~/.claude-personal".to_string());
        })
        .unwrap();
        let target = home
            .path()
            .join(".claude-personal/sandbox-v2/.credentials.json");
        let mut session = crate::session::Instance::new("personal", "/tmp/aoe-login-api-command");
        session.view = crate::session::View::Structured;
        session.tool = "personal".to_string();
        session.command = "claude".to_string();
        session.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-login-api-command".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        let blocked = session.ensure_sandbox_login().unwrap_err();
        assert_eq!(blocked.credential, target, "the gate waits on this file");
        assert_eq!(blocked.command.as_deref(), Some("claude"));
        let session_id = session.id.clone();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            vec![session],
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        let login = &state.session_service.sandbox_login;
        login.hold(
            &session_id,
            crate::server::sandbox_login::LaunchOrigin::Interactive,
            &blocked,
        );
        let fake = Arc::new(Fake::at(tempfile::TempDir::new().unwrap(), target.clone()));
        let started_for = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let recorded = Arc::clone(&started_for);
        login.set_test_starter(move |_profile, tool, command| {
            recorded
                .lock()
                .unwrap()
                .push((tool.to_string(), command.map(str::to_string)));
            let fake = Arc::clone(&fake);
            Box::pin(async move { fake.start_timed(true, HelperTimeouts::default()).await })
        });
        let app = crate::server::test_support::build_router_for_test(Arc::clone(&state));
        let plain = format!("{BASE}?tool=personal");
        let with_command = format!("{BASE}?tool=personal&command=claude");

        // Without the command the tool is the profile's codex agent: no login to read.
        let (_, view) = call(&app, Method::GET, &plain, None).await;
        assert_eq!(view["status"]["state"], "not_applicable");
        assert_eq!(view["held_sessions"], 0);
        let (_, view) = call(&app, Method::GET, &with_command, None).await;
        assert_eq!(view["status"]["state"], "needed", "{view}");
        assert_eq!(view["held_sessions"], 1);

        // A command names the tool it is for, as `--command` requires `--tool`.
        let (status, body) = call(&app, Method::GET, &format!("{BASE}?command=claude"), None).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (
                StatusCode::BAD_REQUEST,
                Some("sandbox_login_command_needs_tool")
            )
        );
        let (status, body) = call(
            &app,
            Method::POST,
            BASE,
            Some(json!({ "command": "claude" }).to_string()),
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (
                StatusCode::BAD_REQUEST,
                Some("sandbox_login_command_needs_tool")
            )
        );

        // The start without the command has no login to run; with it, the helper gets it.
        let (status, body) = call(
            &app,
            Method::POST,
            BASE,
            Some(json!({ "tool": "personal" }).to_string()),
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (
                StatusCode::BAD_REQUEST,
                Some("sandbox_login_tool_unsupported")
            )
        );
        assert!(started_for.lock().unwrap().is_empty());
        let body = json!({ "tool": "personal", "command": "claude" }).to_string();
        let (status, started) = call(&app, Method::POST, BASE, Some(body.clone())).await;
        assert_eq!(status, StatusCode::OK, "{started}");
        assert_eq!(
            *started_for.lock().unwrap(),
            [("personal".to_string(), Some("claude".to_string()))]
        );
        let login_id = started["login_id"].as_str().unwrap().to_string();
        let (_, view) = call(&app, Method::GET, &with_command, None).await;
        assert_eq!(view["running"]["login_id"], login_id.as_str());
        let (_, again) = call(&app, Method::POST, BASE, Some(body)).await;
        assert_eq!(
            again["login_id"],
            login_id.as_str(),
            "the same file's login"
        );

        let (status, _) = call(
            &app,
            Method::POST,
            &format!("{BASE}/{login_id}/code"),
            Some(json!({ "code": "good-code#good-state" }).to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, view) = call(&app, Method::GET, &with_command, None).await;
        assert_eq!(view["status"]["state"], "ok");
        reconcile_once(&state).await;
        assert!(!login.is_held(&session_id), "the login released it");
    }

    /// A session of another profile waits on that profile's file: the status and the start take
    /// the same `profile` as `aoe sandbox login -p` and read and log in that profile's file,
    /// not the daemon's. Only `work` declares the config directory of `claude`.
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_profile_selects_the_file_of_that_profile() {
        use std::sync::Mutex;

        type Started = Vec<(String, String, Option<String>)>;
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        crate::session::config::update_config(|c| {
            c.sandbox.shared_credential =
                crate::session::config::SharedCredentialMode::SeparateLogin;
        })
        .unwrap();
        let mut work = crate::session::config::profile_config::ProfileConfig::default();
        work.overrides.insert(
            "session".to_string(),
            json!({ "agent_config_dir": { "claude": "~/.claude-work" } }),
        );
        crate::session::config::profile_config::save_profile_config("work", &work).unwrap();
        let daemon_file = home.path().join(".claude/sandbox-v2/.credentials.json");
        let work_file = home
            .path()
            .join(".claude-work/sandbox-v2/.credentials.json");
        std::fs::create_dir_all(daemon_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(work_file.parent().unwrap()).unwrap();
        crate::session::test_support::write_fake_shared_credential(&daemon_file);
        std::fs::write(&work_file, "{}").unwrap();

        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        let login = &state.session_service.sandbox_login;
        let fake = Arc::new(Fake::at(tempfile::TempDir::new().unwrap(), work_file));
        let started_for = Arc::new(Mutex::new(Started::new()));
        let recorded = Arc::clone(&started_for);
        login.set_test_starter(move |profile, tool, command| {
            recorded.lock().unwrap().push((
                profile.to_string(),
                tool.to_string(),
                command.map(str::to_string),
            ));
            let fake = Arc::clone(&fake);
            Box::pin(async move { fake.start_timed(true, HelperTimeouts::default()).await })
        });
        let app = crate::server::test_support::build_router_for_test(Arc::clone(&state));
        let state_of =
            |view: &serde_json::Value| view["status"]["state"].as_str().unwrap().to_string();

        // The daemon's profile has a usable file; `work` needs the login, with and without
        // the tool (the profile's default tool resolves as `--tool` does under `-p`).
        let (_, view) = call(&app, Method::GET, &format!("{BASE}?tool=claude"), None).await;
        assert_eq!(state_of(&view), "ok", "{view}");
        for uri in [
            format!("{BASE}?tool=claude&profile=work"),
            format!("{BASE}?profile=work"),
        ] {
            let (status, view) = call(&app, Method::GET, &uri, None).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(state_of(&view), "needed", "{uri}: {view}");
        }
        // An empty profile is the daemon's, as an empty command is the profile's.
        let (_, view) = call(
            &app,
            Method::GET,
            &format!("{BASE}?tool=claude&profile="),
            None,
        )
        .await;
        assert_eq!(state_of(&view), "ok", "{view}");

        // A name that is no profile, or not even a name, is refused before anything runs.
        for bad in ["ghost", "..%2Fwork", "a%20b"] {
            let (status, body) =
                call(&app, Method::GET, &format!("{BASE}?profile={bad}"), None).await;
            assert_eq!(
                (status, body["error"].as_str()),
                (
                    StatusCode::BAD_REQUEST,
                    Some("sandbox_login_unknown_profile")
                ),
                "GET {bad}"
            );
            let body =
                json!({ "tool": "claude", "profile": bad.replace("%2F", "/").replace("%20", " ") });
            let (status, body) = call(&app, Method::POST, BASE, Some(body.to_string())).await;
            assert_eq!(
                (status, body["error"].as_str()),
                (
                    StatusCode::BAD_REQUEST,
                    Some("sandbox_login_unknown_profile")
                ),
                "POST {bad}"
            );
        }
        assert!(started_for.lock().unwrap().is_empty());

        // The start runs the helper for that profile, with the command next to it.
        let body = json!({ "tool": "claude", "command": "claude", "profile": "work" });
        let (status, started) = call(&app, Method::POST, BASE, Some(body.to_string())).await;
        assert_eq!(status, StatusCode::OK, "{started}");
        assert_eq!(
            *started_for.lock().unwrap(),
            [(
                "work".to_string(),
                "claude".to_string(),
                Some("claude".to_string())
            )]
        );
        let login_id = started["login_id"].as_str().unwrap().to_string();
        let (_, view) = call(
            &app,
            Method::GET,
            &format!("{BASE}?tool=claude&command=claude&profile=work"),
            None,
        )
        .await;
        assert_eq!(view["running"]["login_id"], login_id.as_str());
        let (_, view) = call(&app, Method::GET, &format!("{BASE}?tool=claude"), None).await;
        assert!(
            view["running"].is_null(),
            "not the daemon file's login: {view}"
        );
        // The same file's login is the running one; the daemon profile's file is another.
        let body = json!({ "tool": "claude", "profile": "work" });
        let (_, again) = call(&app, Method::POST, BASE, Some(body.to_string())).await;
        assert_eq!(again["login_id"], login_id.as_str());
        let body = json!({ "tool": "claude", "force": true });
        let (status, busy) = call(&app, Method::POST, BASE, Some(body.to_string())).await;
        assert_eq!(
            (status, busy["error"].as_str()),
            (StatusCode::CONFLICT, Some("sandbox_login_busy"))
        );
    }

    /// Apple Container cannot run the login helper: a start is refused at once with the
    /// setting's message under PR 1's code, and the status reads not applicable with the
    /// same reason instead of offering a login.
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_refuses_on_apple_container() {
        let home = crate::session::test_support::isolate_app_dir();
        let (_runtime, _calls) =
            crate::session::test_support::failing_container_runtime(home.path());
        let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-login-api-apple");
        inst.view = crate::session::View::Structured;
        let shared = crate::session::test_support::sandbox_needing_login(home.path(), &mut inst);
        crate::session::config::update_config(|c| {
            c.sandbox.container_runtime =
                crate::session::config::ContainerRuntimeName::AppleContainer;
        })
        .unwrap();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            vec![inst],
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        // A helper that would start must never be reached.
        let shared_path = shared.clone();
        let fake = Arc::new(Fake::at(tempfile::TempDir::new().unwrap(), shared));
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = Arc::clone(&started);
        state
            .session_service
            .sandbox_login
            .set_test_starter(move |_profile, _tool, _command| {
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
                let fake = Arc::clone(&fake);
                Box::pin(async move { fake.start_timed(true, HelperTimeouts::default()).await })
            });
        let app = crate::server::test_support::build_router_for_test(Arc::clone(&state));
        let reason = crate::session::SandboxLoginUnsupported.to_string();

        let (status, view) = call(&app, Method::GET, BASE, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["status"]["state"], "not_applicable");
        assert_eq!(view["reason"], reason.as_str());
        assert!(view["running"].is_null());
        for body in ["{}", r#"{"tool":"claude"}"#, r#"{"force":true}"#] {
            let (status, body) = call(&app, Method::POST, BASE, Some(body.to_string())).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["error"], "sandbox_login_unsupported");
            assert_eq!(body["message"], reason.as_str());
        }
        // A usable login needs no helper: the runtime only matters when a login is wanted, as
        // in the launch gate and `sandbox_login_status`.
        let expires_at = chrono::Utc::now().timestamp_millis() + 3_600_000;
        std::fs::write(
            &shared_path,
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"fake-access","refreshToken":"fake-refresh","expiresAt":{expires_at}}}}}"#
            ),
        )
        .unwrap();
        let (_, view) = call(&app, Method::GET, BASE, None).await;
        assert_eq!(view["status"]["state"], "ok");
        assert!(view.get("reason").is_none(), "{view}");
        let (status, body) = call(&app, Method::POST, BASE, Some("{}".to_string())).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::CONFLICT, Some("sandbox_login_already_ok"))
        );
        let (status, body) = call(
            &app,
            Method::POST,
            BASE,
            Some(r#"{"force":true}"#.to_string()),
        )
        .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::CONFLICT, Some("sandbox_login_unsupported")),
            "a forced login still needs the helper"
        );
        assert!(
            !started.load(std::sync::atomic::Ordering::SeqCst),
            "no helper was started"
        );
        assert!(state.session_service.sandbox_login.running().is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_login_api_start_refuses_outside_separate_login() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vecs(&["127.0.0.1"]),
            Vec::new(),
            Some(TOKEN.to_string()),
        );
        let app = crate::server::test_support::build_router_for_test(state);
        let (status, body) = call(&app, Method::POST, BASE, Some("{}".to_string())).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "sandbox_login_not_separate_login");
        let (_, view) = call(&app, Method::GET, BASE, None).await;
        assert_eq!(view["status"]["state"], "not_applicable");
    }

    #[tokio::test]
    async fn sandbox_login_api_maps_every_helper_error() {
        let cases = [
            (LoginError::Busy, 409, "sandbox_login_busy"),
            (
                LoginError::NotSeparateLogin,
                409,
                "sandbox_login_not_separate_login",
            ),
            (LoginError::AlreadyLoggedIn, 409, "sandbox_login_already_ok"),
            (
                LoginError::Unsupported(crate::session::SandboxLoginUnsupported),
                409,
                "sandbox_login_unsupported",
            ),
            (
                LoginError::NoUrl {
                    output: "o".to_string(),
                },
                422,
                "sandbox_login_no_url",
            ),
            (LoginError::Timeout, 504, "sandbox_login_timeout"),
            (LoginError::Cancelled, 409, "sandbox_login_cancelled"),
            (
                LoginError::Failed {
                    message: "m".to_string(),
                },
                422,
                "login_failed",
            ),
            (LoginError::Gone, 404, "login_not_found"),
            (LoginError::Other(anyhow::anyhow!("boom")), 500, "internal"),
        ];
        for (error, status, code) in cases {
            let resp = login_error_response(error);
            assert_eq!(resp.status().as_u16(), status, "{code}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"], code);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn sandbox_login_api_logs_no_secrets() {
        let logs = crate::session::test_support::LogCapture::start();
        let env = env();
        let (status, started) = start(&env).await;
        assert_eq!(status, StatusCode::OK);
        let login_id = started["login_id"].as_str().unwrap().to_string();

        let invalid = "INVALIDSECRET has spaces";
        // The fake prints the code and the link back before it fails.
        let echoed = "echo-SECRETCODE";
        let (status, invalid_body) = send_code(&env, &login_id, invalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, echoed_body) = send_code(&env, &login_id, echoed).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (_, second) = start(&env).await;
        let good = "good-code#good-state";
        let (status, good_body) = send_code(&env, second["login_id"].as_str().unwrap(), good).await;
        assert_eq!(status, StatusCode::OK);

        // The start response legitimately carries the link, so it is not checked.
        let shown = format!("{invalid_body}{echoed_body}{good_body}{}", logs.contents());
        for leak in [
            invalid,
            "INVALIDSECRET",
            echoed,
            good,
            "good-state",
            MEASURED_URL,
            "SAMPLEstate",
            "SAMPLEchallenge",
        ] {
            assert!(!shown.contains(leak), "leaked {leak:?}");
        }
        assert!(
            logs.contents().contains("http.request"),
            "the request log ran"
        );
    }
}
