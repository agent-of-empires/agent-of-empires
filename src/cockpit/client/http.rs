//! HTTP client for the cockpit daemon.
//!
//! One `HttpClient` per `DaemonEndpoint`; methods map 1:1 to the
//! per-session cockpit REST surface (`/api/sessions/{id}/cockpit/*`).
//! Auth: the endpoint's optional `token` is sent as
//! `Authorization: Bearer <token>` on every request, never as a
//! query string, so it doesn't leak via logs or `ps`.

use std::time::Duration;

use reqwest::{header, StatusCode};
use thiserror::Error;

use super::discovery::DaemonEndpoint;
use crate::cockpit::protocol::{
    ApprovalDecisionWire, ContextPrimerResponse, FilesResponse, PromptRequest, ReplayResponse,
    ResolveApprovalRequest, SwitchAgentRequest, SwitchAgentResponse,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Subset of the daemon's `GET /api/about` payload the cockpit client
/// reads. The full `ServerAbout` carries many more fields; serde drops
/// the rest.
#[derive(serde::Deserialize)]
struct AboutResponse {
    #[serde(default)]
    cockpit_queue_drain_mode: String,
}

/// Page size requested by [`HttpClient::replay_paged`]. Stays at or
/// under the server's `MAX_REPLAY_PAGE` so it is never clamped down.
pub const REPLAY_PAGE_SIZE: u64 = 1000;

/// Cockpit daemon HTTP client. Cheap to clone; the underlying
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
    #[error("cockpit session {0} not found on the daemon")]
    SessionNotFound(String),
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
            .user_agent(concat!("aoe-cockpit-client/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, endpoint })
    }

    pub fn endpoint(&self) -> &DaemonEndpoint {
        &self.endpoint
    }

    /// `GET /api/sessions/{id}/cockpit/replay?since=N`. Unbounded fetch
    /// (no `limit`): the server still applies its default page bound, so
    /// this returns at most one page. Used by the status probe, which
    /// only reads the metadata (`highest_seq`/`lowest_seq`) and passes
    /// `since=u64::MAX` so no frames come back. History consumers should
    /// use [`replay_paged`](Self::replay_paged) instead.
    pub async fn replay(&self, session_id: &str, since: u64) -> Result<ReplayResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/replay?since={}",
            self.endpoint.base_url, session_id, since
        );
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<ReplayResponse>().await?)
    }

    /// `GET /api/sessions/{id}/cockpit/replay?since=N&limit=L`. One page.
    pub async fn replay_page(
        &self,
        session_id: &str,
        since: u64,
        limit: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/replay?since={}&limit={}",
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

    /// `GET /api/sessions/{id}/cockpit/context-primer?before_seq=N`.
    pub async fn context_primer(
        &self,
        session_id: &str,
        before_seq: u64,
    ) -> Result<ContextPrimerResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/context-primer?before_seq={}",
            self.endpoint.base_url, session_id, before_seq
        );
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<ContextPrimerResponse>().await?)
    }

    /// `GET /api/sessions/{id}/cockpit/files`. Workspace file list for
    /// the composer's `@`-mention picker.
    pub async fn files(&self, session_id: &str) -> Result<FilesResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/files",
            self.endpoint.base_url, session_id
        );
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, session_id).await?;
        Ok(res.json::<FilesResponse>().await?)
    }

    /// `POST /api/sessions/{id}/cockpit/prompt`.
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/prompt",
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

    /// `GET /api/about`. Returns the daemon's resolved
    /// `cockpit.queue_drain_mode`, which the TUI cockpit needs because it
    /// may attach to a remote daemon whose config differs from the local
    /// machine's. Unknown / unparseable values fall back to the default.
    pub async fn queue_drain_mode(
        &self,
    ) -> Result<crate::session::config::QueueDrainMode, HttpError> {
        let url = format!("{}/api/about", self.endpoint.base_url);
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, "<about>").await?;
        let about = res.json::<AboutResponse>().await?;
        Ok(
            crate::session::config::QueueDrainMode::parse(&about.cockpit_queue_drain_mode)
                .unwrap_or_default(),
        )
    }

    /// `POST /api/sessions/{id}/cockpit/cancel`.
    pub async fn cancel(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/cancel",
            self.endpoint.base_url, session_id
        );
        let res = self.auth(self.http.post(&url)).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/cockpit/switch-agent`. Hands the session
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
            "{}/api/sessions/{}/cockpit/switch-agent",
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

    /// `POST /api/sessions/{id}/cockpit/approvals/{nonce}`.
    pub async fn resolve_approval(
        &self,
        session_id: &str,
        nonce: &str,
        decision: ApprovalDecisionWire,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/cockpit/approvals/{}",
            self.endpoint.base_url, session_id, nonce
        );
        let body = ResolveApprovalRequest { decision };
        let res = self.auth(self.http.post(&url)).json(&body).send().await?;
        check_status(res, session_id).await?;
        Ok(())
    }

    /// `GET /api/sessions`. Returns the daemon's session list as
    /// whatever shape the caller deserialises into. Used by the
    /// remote-cockpit picker so the bespoke `reqwest::Client` it used
    /// to keep can be retired in favour of the shared auth/header
    /// plumbing.
    pub async fn list_sessions<T: serde::de::DeserializeOwned>(&self) -> Result<Vec<T>, HttpError> {
        let url = format!("{}/api/sessions", self.endpoint.base_url);
        let res = self.auth(self.http.get(&url)).send().await?;
        let res = check_status(res, "<sessions>").await?;
        Ok(res.json::<Vec<T>>().await?)
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
    match status {
        StatusCode::UNAUTHORIZED => Err(HttpError::Unauthorized),
        StatusCode::FORBIDDEN if body.contains("read-only") || body.contains("read_only") => {
            Err(HttpError::ReadOnly)
        }
        StatusCode::NOT_FOUND => Err(HttpError::SessionNotFound(session_id.to_string())),
        _ => Err(HttpError::Server { status, body }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cockpit::client::discovery::Source;

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
        assert_eq!(client.endpoint().token.as_deref(), Some("tok"));
    }

    #[test]
    fn auth_skips_bearer_when_no_token() {
        let client = HttpClient::new(endpoint("http://127.0.0.1:8080", None)).unwrap();
        assert!(client.endpoint().token.is_none());
    }

    // Regression test for #1525. The startup toast on a 401 from the
    // cockpit endpoints folds in `HttpError::Unauthorized`'s Display.
    // Previously that Display string hard-coded `AOE_DAEMON_TOKEN`,
    // which made the toast actively misleading on `--auth=passphrase`
    // and `--auth=none` daemons that never had a token. Pin the new
    // wording so the env-var hint can't regress back in.
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
