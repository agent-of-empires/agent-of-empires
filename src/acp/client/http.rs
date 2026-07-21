//! HTTP client for the structured view daemon.
//!
//! One `HttpClient` per `DaemonEndpoint`; methods map 1:1 to the
//! per-session structured view REST surface (`/api/sessions/{id}/acp/*`).
//! Auth: the endpoint's optional `token` is sent as
//! `Authorization: Bearer <token>` on every request, never as a
//! query string, so it doesn't leak via logs or `ps`.

use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{header, StatusCode};
use thiserror::Error;

use super::discovery::DaemonEndpoint;
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::protocol::{
    ApprovalDecisionWire, FilesResponse, PromptRequest, ReplayResponse, ResolveApprovalRequest,
    SwitchAgentRequest, SwitchAgentResponse,
};
use crate::acp::session_paths::SessionPathRoots;
use crate::plugin::ui_state::UiSnapshot;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Percent-encode set for a single URL path segment. A well-formed fqid
/// (`plugin.<id>.<command>`, dotted lowercase) is left intact so it round-trips
/// to the same string server-side, while structurally dangerous bytes (`/`,
/// `?`, `#`, `%`, space, controls) are escaped so a malformed id can never
/// break out of its path segment.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

#[derive(serde::Deserialize)]
struct SessionsEnvelope<T> {
    sessions: Vec<T>,
}

/// One active plugin command as the daemon reports it (`GET
/// /api/plugins/commands`), the source of truth the structured view resolves
/// keybinds against: for a session on a remote daemon the plugin may not be
/// installed on the TUI's own machine, so its local registry cannot resolve or
/// execute it. Mirrors the server's `PluginCommandView`; only the execution
/// fields are kept.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginCommandView {
    pub fqid: String,
    pub plugin_id: String,
    #[serde(default)]
    pub keybinds: Vec<String>,
    #[serde(default)]
    pub action: Option<aoe_plugin_api::ClientAction>,
}

#[derive(serde::Deserialize)]
struct PluginCommandsEnvelope {
    commands: Vec<PluginCommandView>,
}

/// Page size requested by [`HttpClient::replay_paged`]. Stays at or
/// under the server's `MAX_REPLAY_PAGE` so it is never clamped down.
pub const REPLAY_PAGE_SIZE: u64 = 1000;

/// Acp daemon HTTP client. Cheap to clone; the underlying
/// `reqwest::Client` is reference-counted.
#[derive(Debug, Clone)]
pub struct HttpClient {
    http: reqwest::Client,
    endpoint: DaemonEndpoint,
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("structured view session {0} not found on the daemon")]
    SessionNotFound(String),
    // A 404 whose body names the missing nonce: the approval already
    // resolved server-side (concurrent decision, watchdog cancel, or the
    // agent offered no matching option). Distinct from SessionNotFound so
    // the approval flow can clear the card instead of toasting an error.
    // See #1821.
    #[error("approval already resolved")]
    ApprovalGone,
    #[error("daemon is read-only (started with --read-only); request refused")]
    ReadOnly,
    // The daemon may reject for several reasons: stale token, missing
    // passphrase session, device binding mismatch. Pointing at
    // `AOE_DAEMON_TOKEN` was misleading on `--auth=passphrase` and
    // `--auth=none` daemons that never had a token in the first
    // place. See #1525.
    #[error("daemon rejected the request (401); restart `aoe serve` or check `--auth` mode")]
    Unauthorized,
    #[error("daemon returned HTTP {status}: {body}")]
    Server { status: StatusCode, body: String },
}

impl HttpClient {
    pub fn new(endpoint: DaemonEndpoint) -> Result<Self, HttpError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(concat!("aoe-acp-client/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, endpoint })
    }

    /// `GET /api/sessions/{id}/acp/replay?since=N`. Unbounded fetch
    /// (no `limit`): the server still applies its default page bound, so
    /// this returns at most one page. Used by the status probe, which
    /// only reads the metadata (`highest_seq`/`lowest_seq`) and passes
    /// `since=u64::MAX` so no frames come back. History consumers should
    /// use [`replay_paged`](Self::replay_paged) instead.
    pub async fn replay(&self, session_id: &str, since: u64) -> Result<ReplayResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/replay?since={}",
            self.endpoint.base_url, session_id, since
        );
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<ReplayResponse>().await?)
    }

    /// `GET /api/sessions/{id}/acp/replay?since=N&limit=L`. One page.
    pub async fn replay_page(
        &self,
        session_id: &str,
        since: u64,
        limit: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/replay?since={}&limit={}",
            self.endpoint.base_url, session_id, since, limit
        );
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<ReplayResponse>().await?)
    }

    /// Page through replay history from `since`, accumulating every
    /// frame into one `ReplayResponse`. Each request is bounded to
    /// `page_size` so the daemon never buffers the whole history at once.
    ///
    /// The loop is capped at the first page's `highest_seq`: events
    /// appended after replay began arrive over the live WS channel and
    /// are deduped by the reducer, so chasing them here would never
    /// converge on a busy session. Stops early and propagates `lost` if
    /// any page reports a retention gap, leaving the caller to reset.
    pub async fn replay_paged(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let mut frames = Vec::new();
        let mut cursor = since;
        let mut target: Option<u64> = None;
        let mut lost = false;
        // Assigned every iteration before the post-loop read; the loop
        // always runs at least once.
        let mut highest_seq;
        let mut lowest_seq;
        loop {
            let page = self.replay_page(session_id, cursor, page_size).await?;
            highest_seq = page.highest_seq;
            lowest_seq = page.lowest_seq;
            let cap = *target.get_or_insert(page.highest_seq);
            frames.extend(page.frames);
            if page.lost {
                lost = true;
                break;
            }
            match page.next_cursor {
                // Keep paging only while the cursor advances and stays
                // within the snapshot window captured on the first page.
                Some(next) if page.has_more && next > cursor && next < cap => {
                    cursor = next;
                }
                _ => break,
            }
        }
        Ok(ReplayResponse {
            frames,
            lost,
            highest_seq,
            lowest_seq,
            next_cursor: None,
            has_more: false,
        })
    }

    /// `GET /api/sessions/{id}/acp/files`. Workspace file list for
    /// the composer's `@`-mention picker.
    pub async fn files(&self, session_id: &str) -> Result<FilesResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/files",
            self.endpoint.base_url, session_id
        );
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<FilesResponse>().await?)
    }

    /// `POST /api/sessions/{id}/acp/prompt`.
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/prompt",
            self.endpoint.base_url, session_id
        );
        let body = PromptRequest {
            text: text.to_string(),
            attachments: Vec::new(),
        };
        let res = self.auth(self.http.post(&url)).json(&body).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `GET /api/plugins/ui-state`. The daemon-wide plugin UI snapshot
    /// (host-rendered slots + notifications) the web dashboard polls; the
    /// native structured view renders the TUI-applicable subset (#2402).
    /// Global, not session-scoped, so a miss must not be classified as a
    /// session-not-found.
    pub async fn plugin_ui_state(&self) -> Result<UiSnapshot, HttpError> {
        let url = format!("{}/api/plugins/ui-state", self.endpoint.base_url);
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_global_status(res).await?;
        Ok(res.json::<UiSnapshot>().await?)
    }

    /// `GET /api/plugins/commands`. The daemon's active plugin commands with
    /// their keybinds and client actions. The structured view resolves plugin
    /// chords against this rather than the TUI's local registry, so a session on
    /// a remote daemon can drive plugins installed only there. Global, like
    /// `plugin_ui_state`.
    pub async fn plugin_commands(&self) -> Result<Vec<PluginCommandView>, HttpError> {
        let url = format!("{}/api/plugins/commands", self.endpoint.base_url);
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_global_status(res).await?;
        Ok(res.json::<PluginCommandsEnvelope>().await?.commands)
    }

    /// `POST /api/plugins/commands/{fqid}/invoke`. Dispatch an action-less
    /// plugin command to its worker as a fire-and-forget notification (the TUI
    /// twin of the web palette's invoke). Global, like `plugin_ui_state`; the
    /// daemon validates the command and session. `fqid` has no slashes, so it
    /// is a single path segment.
    pub async fn invoke_plugin_command(
        &self,
        fqid: &str,
        session_id: &str,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/plugins/commands/{}/invoke",
            self.endpoint.base_url,
            utf8_percent_encode(fqid, PATH_SEGMENT)
        );
        let body = serde_json::json!({ "session_id": session_id });
        let res = self.auth(self.http.post(&url)).json(&body).send().await?;
        check_global_status(res).await?;
        Ok(())
    }

    /// `POST /api/plugins/{id}/enabled`. Toggling through the daemon (rather
    /// than writing config locally) lets its plugin host reconcile workers
    /// live: enabling launches the worker, disabling tears it down. Global,
    /// like `plugin_ui_state`.
    pub async fn set_plugin_enabled(
        &self,
        plugin_id: &str,
        enabled: bool,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/plugins/{}/enabled",
            self.endpoint.base_url, plugin_id
        );
        let res = self
            .auth(self.http.post(&url))
            .json(&serde_json::json!({ "enabled": enabled }))
            .send()
            .await?;
        check_global_status(res).await?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/cancel`.
    pub async fn cancel(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/cancel",
            self.endpoint.base_url, session_id
        );
        let res = self.auth(self.http.post(&url)).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/mode`: set the active session
    /// permission mode (an ACP `session/set_mode` round-trip). The new
    /// mode echoes back over the WebSocket as `CurrentModeChanged`;
    /// a rejection surfaces as `ModeSwitchFailed`.
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/mode",
            self.endpoint.base_url, session_id
        );
        let body = serde_json::json!({ "mode_id": mode_id });
        let res = self.auth(self.http.post(&url)).json(&body).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/enable`: switch a terminal-mode
    /// session to the structured view. The daemon tears down the tmux
    /// pane, persists `view = Structured`, and its reconciler spawns the
    /// ACP worker. Idempotent when the session is already structured.
    pub async fn acp_enable(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/enable",
            self.endpoint.base_url, session_id
        );
        let res = self.auth(self.http.post(&url)).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/disable`: switch a structured-view
    /// session back to a tmux terminal. The daemon stops the worker and
    /// persists `view = Terminal`; the next attach spawns the pane.
    /// Idempotent when the session is already terminal.
    pub async fn acp_disable(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/disable",
            self.endpoint.base_url, session_id
        );
        let res = self.auth(self.http.post(&url)).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/switch-agent`. Hands the session
    /// off to another ACP backend, keeping the transcript. Returns the
    /// daemon's response (before/switch seqs) so callers can fetch a
    /// context primer if they want a handoff recap.
    pub async fn switch_agent(
        &self,
        session_id: &str,
        target: &str,
        model: Option<&str>,
        reason: Option<&str>,
    ) -> Result<SwitchAgentResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/switch-agent",
            self.endpoint.base_url, session_id
        );
        let body = SwitchAgentRequest {
            target: target.to_string(),
            model: model.map(str::to_string),
            reason: reason.map(str::to_string),
        };
        let res = self.auth(self.http.post(&url)).json(&body).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<SwitchAgentResponse>().await?)
    }

    /// `POST /api/sessions/{id}/acp/approvals/{nonce}`.
    pub async fn resolve_approval(
        &self,
        session_id: &str,
        nonce: &str,
        decision: ApprovalDecisionWire,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/approvals/{}",
            self.endpoint.base_url, session_id, nonce
        );
        let body = ResolveApprovalRequest { decision };
        let res = self.auth(self.http.post(&url)).json(&body).send().await?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let text = res.text().await.unwrap_or_default();
        Err(classify_resolve_error(status, &text, nonce, session_id))
    }

    /// `POST /api/sessions/{id}/acp/elicitations/{nonce}`. The native TUI
    /// only ever sends decline/cancel (the rich answer form is web-only),
    /// but the body is the full `ElicitationResolution` so the same client
    /// could submit answers.
    pub async fn resolve_elicitation(
        &self,
        session_id: &str,
        nonce: &str,
        resolution: &ElicitationResolution,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/elicitations/{}",
            self.endpoint.base_url, session_id, nonce
        );
        let res = self
            .auth(self.http.post(&url))
            .json(resolution)
            .send()
            .await?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let text = res.text().await.unwrap_or_default();
        Err(classify_resolve_error(status, &text, nonce, session_id))
    }

    /// `GET /api/sessions`. Returns the daemon's session list as
    /// whatever shape the caller deserialises into. Used by the
    /// remote-structured view picker so the bespoke `reqwest::Client` it used
    /// to keep can be retired in favour of the shared auth/header
    /// plumbing.
    pub async fn list_sessions<T: serde::de::DeserializeOwned>(&self) -> Result<Vec<T>, HttpError> {
        let url = format!("{}/api/sessions", self.endpoint.base_url);
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, "<sessions>").await?;
        Ok(res.json::<SessionsEnvelope<T>>().await?.sessions)
    }

    /// Session root paths used to render tool-call file labels relative to the
    /// active worktree or workspace repository.
    pub async fn session_path_roots(
        &self,
        session_id: &str,
    ) -> Result<SessionPathRoots, HttpError> {
        let sessions = self.list_sessions::<SessionPathRoots>().await?;
        sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| HttpError::SessionNotFound(session_id.to_string()))
    }

    /// Lightweight reachability probe used by `require_daemon` (when
    /// `AOE_DAEMON_URL` is set, we fail loud before falling into raw
    /// reqwest transport errors) and `aoe serve --status` (renders
    /// remote daemon info instead of "Daemon: not running").
    ///
    /// Hits `GET /api/sessions`, the cheapest authenticated endpoint
    /// in the surface; succeeds with 200 when the daemon is up *and*
    /// the token is valid, separates "host is down" (transport error)
    /// from "auth misconfigured" (401).
    pub async fn health_check(&self) -> Result<(), HttpError> {
        let url = format!("{}/api/sessions", self.endpoint.base_url);
        let res = self.auth(self.http.get(&url)).send().await?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let body = res.text().await.unwrap_or_default();
        match status {
            StatusCode::UNAUTHORIZED => Err(HttpError::Unauthorized),
            _ => Err(HttpError::Server { status, body }),
        }
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.endpoint.token {
            Some(token) => builder.header(header::AUTHORIZATION, format!("Bearer {token}")),
            None => builder,
        }
    }
}

async fn check_status(
    res: reqwest::Response,
    session_id: &str,
) -> Result<reqwest::Response, HttpError> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let body = res.text().await.unwrap_or_default();
    Err(classify_error(status, &body, session_id))
}

/// Status check for daemon-wide (non-session) endpoints. Like
/// [`check_status`] but never mints `SessionNotFound`: a 404 here means the
/// route is absent (e.g. an older daemon without the plugin UI endpoint),
/// not a missing session, so it maps to a plain `Server` error.
async fn check_global_status(res: reqwest::Response) -> Result<reqwest::Response, HttpError> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let body = res.text().await.unwrap_or_default();
    match status {
        StatusCode::UNAUTHORIZED => Err(HttpError::Unauthorized),
        StatusCode::FORBIDDEN if body.contains("read-only") || body.contains("read_only") => {
            Err(HttpError::ReadOnly)
        }
        _ => Err(HttpError::Server { status, body }),
    }
}

/// Map a non-success daemon response onto a typed error. Split out from
/// `check_status` so the status/body dispatch is unit-testable without a
/// live `reqwest::Response`.
fn classify_error(status: StatusCode, body: &str, session_id: &str) -> HttpError {
    match status {
        StatusCode::UNAUTHORIZED => HttpError::Unauthorized,
        StatusCode::FORBIDDEN if body.contains("read-only") || body.contains("read_only") => {
            HttpError::ReadOnly
        }
        StatusCode::NOT_FOUND => HttpError::SessionNotFound(session_id.to_string()),
        _ => HttpError::Server {
            status,
            body: body.to_string(),
        },
    }
}

/// Classify the response of an approval- or elicitation-resolve POST.
/// Scoped to those endpoints (not the shared `check_status`, which
/// replay/prompt/cancel use too) so only this path can mint `ApprovalGone`.
/// A 404 whose body names *this* nonce means the pending approval or
/// elicitation already resolved server-side (a concurrent decision, a
/// watchdog, or a torn-down request); the caller clears the card quietly
/// rather than surfacing an error. Anything else folds back into the
/// generic classifier. See #1821.
fn classify_resolve_error(
    status: StatusCode,
    body: &str,
    nonce: &str,
    session_id: &str,
) -> HttpError {
    let names_gone_target =
        body.contains("no pending approval") || body.contains("no pending elicitation");
    if status == StatusCode::NOT_FOUND && names_gone_target && body.contains(nonce) {
        HttpError::ApprovalGone
    } else {
        classify_error(status, body, session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;

    fn endpoint(base: &str, token: Option<&str>) -> DaemonEndpoint {
        DaemonEndpoint {
            base_url: base.to_string(),
            token: token.map(str::to_string),
            source: Source::Env,
        }
    }

    #[test]
    fn auth_sets_bearer_when_token_present() {
        let client = HttpClient::new(endpoint("http://127.0.0.1:8080", Some("tok"))).unwrap();
        // Smoke-check by reading endpoint back; full header inspection
        // requires a live request and lives in the integration tests
        // alongside the axum mock.
        assert_eq!(client.endpoint.token.as_deref(), Some("tok"));
    }

    #[test]
    fn auth_skips_bearer_when_no_token() {
        let client = HttpClient::new(endpoint("http://127.0.0.1:8080", None)).unwrap();
        assert!(client.endpoint.token.is_none());
    }

    // Regression test for #1525. The startup toast on a 401 from the
    // structured view endpoints folds in `HttpError::Unauthorized`'s Display.
    // Previously that Display string hard-coded `AOE_DAEMON_TOKEN`,
    // which made the toast actively misleading on `--auth=passphrase`
    // and `--auth=none` daemons that never had a token. Pin the new
    // wording so the env-var hint can't regress back in.
    #[test]
    fn classify_resolve_error_clears_only_on_matching_nonce() {
        // #1821: ApprovalGone is minted only by the resolve path, only for a
        // 404 that names *this* nonce. A session-gone 404, or a 404 naming a
        // different nonce, stays a real error.
        assert!(matches!(
            classify_resolve_error(
                StatusCode::NOT_FOUND,
                "no pending approval with nonce abc-123",
                "abc-123",
                "s-1"
            ),
            HttpError::ApprovalGone
        ));
        assert!(matches!(
            classify_resolve_error(
                StatusCode::NOT_FOUND,
                "no pending approval with nonce other-999",
                "abc-123",
                "s-1"
            ),
            HttpError::SessionNotFound(s) if s == "s-1"
        ));
        assert!(matches!(
            classify_resolve_error(
                StatusCode::NOT_FOUND,
                "session has no running structured view",
                "abc-123",
                "s-1"
            ),
            HttpError::SessionNotFound(s) if s == "s-1"
        ));
        // A gone elicitation nonce is classified the same as a gone approval.
        assert!(matches!(
            classify_resolve_error(
                StatusCode::NOT_FOUND,
                "no pending elicitation with nonce abc-123",
                "abc-123",
                "s-1"
            ),
            HttpError::ApprovalGone
        ));
    }

    #[test]
    fn classify_error_never_mints_approval_gone() {
        // The shared classifier (used by replay/prompt/cancel/session-list)
        // must not produce ApprovalGone; a bare 404 is a session miss.
        assert!(matches!(
            classify_error(StatusCode::NOT_FOUND, "no pending approval with that nonce", "s-1"),
            HttpError::SessionNotFound(s) if s == "s-1"
        ));
    }

    #[test]
    fn classify_error_maps_auth_and_read_only() {
        assert!(matches!(
            classify_error(StatusCode::UNAUTHORIZED, "", "s-1"),
            HttpError::Unauthorized
        ));
        assert!(matches!(
            classify_error(StatusCode::FORBIDDEN, "daemon is read-only", "s-1"),
            HttpError::ReadOnly
        ));
        assert!(matches!(
            classify_error(StatusCode::INTERNAL_SERVER_ERROR, "boom", "s-1"),
            HttpError::Server { .. }
        ));
    }

    #[test]
    fn unauthorized_display_omits_token_env_var() {
        let rendered = HttpError::Unauthorized.to_string();
        assert!(
            !rendered.contains("AOE_DAEMON_TOKEN"),
            "Unauthorized message must not pin diagnosis to a token env var that does not exist in passphrase or no-auth mode: {rendered}"
        );
        assert!(
            rendered.contains("401"),
            "Unauthorized message should still surface the underlying HTTP status: {rendered}"
        );
    }
}
