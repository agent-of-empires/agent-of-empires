//! HTTP client for the structured view daemon.
//!
//! One `HttpClient` per `DaemonEndpoint`; methods map 1:1 to the
//! per-session structured view REST surface (`/api/sessions/{id}/acp/*`).
//! Auth: the endpoint's optional `token` is sent as
//! `Authorization: Bearer <token>` on every request, never as a
//! query string, so it doesn't leak via logs or `ps`.

use crate::daemon::transport::path_segment;
use reqwest::{header, StatusCode};
use thiserror::Error;

use super::discovery::DaemonEndpoint;
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::protocol::{
    ApprovalDecisionWire, FilesResponse, PromptRequest, ReplayResponse, ResolveApprovalRequest,
    SwitchAgentRequest, SwitchAgentResponse,
};
use crate::plugin::ui_state::UiSnapshot;

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

/// Wire mirror of the daemon's `/acp/prompt` disposition.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct PromptDispatchWire {
    pub disposition: PromptDispositionWire,
    /// The queue row's id, present only on `queued`.
    #[serde(default)]
    pub queued_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDispositionWire {
    #[default]
    Sent,
    Steered,
    Queued,
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
    #[error("daemon HTTP request or response failed")]
    Transport,
    #[error("structured view session {0} not found on the daemon")]
    SessionNotFound(String),
    #[error("approval already resolved")]
    ApprovalGone,
    #[error("daemon is read-only (started with --read-only); request refused")]
    ReadOnly,
    // A 401 can mean token, passphrase or device authentication failed.
    #[error("daemon rejected the request (401); restart `aoe serve` or check `--auth` mode")]
    Unauthorized,
    #[error("daemon returned HTTP {status}: {body}")]
    Server { status: StatusCode, body: String },
    #[error(transparent)]
    Daemon(#[from] crate::daemon::DaemonClientError),
}

impl From<reqwest::Error> for HttpError {
    fn from(_: reqwest::Error) -> Self {
        Self::Transport
    }
}

impl HttpClient {
    pub fn new(endpoint: DaemonEndpoint) -> Result<Self, HttpError> {
        let url = crate::daemon::sessions_url(&endpoint.base_url)?;
        let token = endpoint.bearer_token();
        crate::daemon::authorization_header(token)?;
        let http = crate::daemon::native_http_client(&url, token.is_some())?;
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
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json::<ReplayResponse>(res).await?)
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
            self.endpoint.base_url,
            path_segment(session_id)?,
            since,
            limit
        );
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json::<ReplayResponse>(res).await?)
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
            rows: None,
        })
    }

    /// `GET /api/sessions/{id}/acp/replay?since=N&limit=L&view=rows`. One
    /// page of the server-folded transcript rows (`TranscriptRow[]` in
    /// `rows`, `frames` empty), same pagination metadata as the raw
    /// projection.
    async fn replay_rows_page(
        &self,
        session_id: &str,
        since: u64,
        limit: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/replay?since={}&limit={}&view=rows",
            self.endpoint.base_url,
            path_segment(session_id)?,
            since,
            limit
        );
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json::<ReplayResponse>(res).await?)
    }

    /// Page through the server-folded transcript rows from `since`,
    /// accumulating every page's `rows` in order and reconciling by row id
    /// (the server folds each page in isolation, so a `tool_start` split
    /// across a page seam can repeat under one id; last non-sparse wins).
    /// The transcript twin of [`replay_paged`](Self::replay_paged): same
    /// snapshot-window cap and retention-gap (`lost`) handling. Returns the
    /// merged rows plus whether a page reported a gap.
    pub async fn replay_rows_paged(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
    ) -> Result<(Vec<crate::acp::transcript::TranscriptRow>, bool), HttpError> {
        let mut rows: Vec<crate::acp::transcript::TranscriptRow> = Vec::new();
        let mut cursor = since;
        let mut target: Option<u64> = None;
        let mut lost = false;
        loop {
            let page = self.replay_rows_page(session_id, cursor, page_size).await?;
            let cap = *target.get_or_insert(page.highest_seq);
            if let Some(page_rows) = page.rows {
                for row in page_rows {
                    crate::acp::transcript::upsert_transcript_row(&mut rows, row);
                }
            }
            if page.lost {
                lost = true;
                break;
            }
            match page.next_cursor {
                Some(next) if page.has_more && next > cursor && next < cap => {
                    cursor = next;
                }
                _ => break,
            }
        }
        Ok((rows, lost))
    }

    /// `GET /api/sessions/{id}/acp/files`. Workspace file list for
    /// the composer's `@`-mention picker.
    pub async fn files(&self, session_id: &str) -> Result<FilesResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/files",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json::<FilesResponse>(res).await?)
    }

    /// `POST /api/sessions/{id}/acp/prompt`.
    ///
    /// The response explicitly identifies sent, steered or queued dispatch.
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        no_revive: bool,
    ) -> Result<PromptDispatchWire, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/prompt",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let body = PromptRequest {
            text: text.to_string(),
            attachments: Vec::new(),
            prompt_id: None,
            no_revive,
        };
        let res = self.execute(self.http.post(&url).json(&body)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json::<PromptDispatchWire>(res).await?)
    }

    /// `GET /api/plugins/ui-state`. The daemon-wide plugin UI snapshot
    /// (host-rendered slots + notifications) the web dashboard polls; the
    /// native structured view renders the TUI-applicable subset (#2402).
    /// Global, not session-scoped, so a miss must not be classified as a
    /// session-not-found.
    pub async fn plugin_ui_state(&self) -> Result<UiSnapshot, HttpError> {
        let url = format!("{}/api/plugins/ui-state", self.endpoint.base_url);
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_global_status(res)?;
        Ok(crate::daemon::decode_json::<UiSnapshot>(res).await?)
    }

    /// `GET /api/plugins/commands`. The daemon's active plugin commands with
    /// their keybinds and client actions. The structured view resolves plugin
    /// chords against this rather than the TUI's local registry, so a session on
    /// a remote daemon can drive plugins installed only there. Global, like
    /// `plugin_ui_state`.
    pub async fn plugin_commands(&self) -> Result<Vec<PluginCommandView>, HttpError> {
        let url = format!("{}/api/plugins/commands", self.endpoint.base_url);
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_global_status(res)?;
        Ok(crate::daemon::decode_json::<PluginCommandsEnvelope>(res)
            .await?
            .commands)
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
            path_segment(fqid)?
        );
        let body = serde_json::json!({ "session_id": session_id });
        let res = self.execute(self.http.post(&url).json(&body)).await?;
        check_global_status(res)?;
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
            self.endpoint.base_url,
            path_segment(plugin_id)?
        );
        let res = self
            .execute(
                self.http
                    .post(&url)
                    .json(&serde_json::json!({ "enabled": enabled })),
            )
            .await?;
        check_global_status(res)?;
        Ok(())
    }

    /// `POST /api/plugins/{id}/worker/restart`: the daemon reloads plugins from
    /// disk and replaces this plugin's worker after its tree changed.
    pub async fn restart_plugin_worker(&self, plugin_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/plugins/{}/worker/restart",
            self.endpoint.base_url, plugin_id
        );
        let res = self.execute(self.http.post(&url)).await?;
        check_global_status(res)?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/cancel`.
    pub async fn cancel(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/cancel",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.post(&url)).await?;
        check_status(res, session_id)?;
        Ok(())
    }

    /// `GET /api/sessions/{id}/queue`: the server-owned prompt queue, ordered
    /// by ascending `seq`. The daemon owns and drains this queue, so the native
    /// view mirrors it rather than keeping its own.
    pub async fn queue_list(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::daemon::QueuedPromptEntry>, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/queue",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json_bounded(res, 64 * 1024 * 1024).await?)
    }

    // No `queue_enqueue` here: since Tier 3 the native view never decides to
    // queue. It POSTs every prompt to `/acp/prompt` and the daemon parks it if
    // it must, so a client-side enqueue would be a second, competing decision.
    // `POST /api/sessions/{id}/queue` still exists for the web's own paths.

    /// `PATCH /api/sessions/{id}/queue/{promptId}`: replace a queued prompt's
    /// text in place, keeping its position. The prompt id is client-minted and
    /// may be arbitrary, so it is percent-encoded to a single path segment.
    pub async fn queue_edit(
        &self,
        session_id: &str,
        prompt_id: &str,
        text: &str,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/queue/{}",
            self.endpoint.base_url,
            path_segment(session_id)?,
            path_segment(prompt_id)?
        );
        let body = serde_json::json!({ "text": text });
        let res = self.execute(self.http.patch(&url).json(&body)).await?;
        check_status(res, session_id)?;
        Ok(())
    }

    /// `DELETE /api/sessions/{id}/queue`: drop every queued prompt.
    pub async fn queue_clear(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/queue",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.delete(&url)).await?;
        check_status(res, session_id)?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/smart-rename`: on-demand "Auto-name now" for a
    /// structured session. The daemon forces past the `smart_rename`-disabled
    /// gate and runs the one-shot detached; a 2xx means "started", not
    /// "renamed" (the new title arrives over the structured-view WS).
    pub async fn smart_rename(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/smart-rename",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.post(&url)).await?;
        check_status(res, session_id)?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/mode`: set the active session
    /// permission mode (an ACP `session/set_mode` round-trip). The new
    /// mode echoes back over the WebSocket as `CurrentModeChanged`;
    /// a rejection surfaces as `ModeSwitchFailed`.
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/mode",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let body = serde_json::json!({ "mode_id": mode_id });
        let res = self.execute(self.http.post(&url).json(&body)).await?;
        check_status(res, session_id)?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/enable`: switch a terminal-mode
    /// session to the structured view. The daemon tears down the tmux
    /// pane, persists `view = Structured`, and its reconciler spawns the
    /// ACP worker. Idempotent when the session is already structured.
    pub async fn acp_enable(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/enable",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.post(&url)).await?;
        check_status(res, session_id)?;
        Ok(())
    }

    /// `POST /api/sessions/{id}/acp/disable`: switch a structured-view
    /// session back to a tmux terminal. The daemon stops the worker and
    /// persists `view = Terminal`; the next attach spawns the pane.
    /// Idempotent when the session is already terminal.
    pub async fn acp_disable(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/disable",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let res = self.execute(self.http.post(&url)).await?;
        check_status(res, session_id)?;
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
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let body = SwitchAgentRequest {
            target: target.to_string(),
            model: model.map(str::to_string),
            reason: reason.map(str::to_string),
        };
        let res = self.execute(self.http.post(&url).json(&body)).await?;
        let res = check_status(res, session_id)?;
        Ok(crate::daemon::decode_json::<SwitchAgentResponse>(res).await?)
    }

    /// `POST /api/sessions/{id}/acp/approvals/{nonce}`. `option_id` names
    /// an option the TUI answered through the option picker.
    pub async fn resolve_approval(
        &self,
        session_id: &str,
        nonce: &str,
        decision: ApprovalDecisionWire,
        option_id: Option<String>,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/approvals/{}",
            self.endpoint.base_url,
            path_segment(session_id)?,
            path_segment(nonce)?
        );
        let body = ResolveApprovalRequest {
            decision,
            option_id,
        };
        let res = self.execute(self.http.post(&url).json(&body)).await?;
        check_resolve_status(res, session_id)
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
            self.endpoint.base_url,
            path_segment(session_id)?,
            path_segment(nonce)?
        );
        let res = self.execute(self.http.post(&url).json(resolution)).await?;
        check_resolve_status(res, session_id)
    }

    /// Session title, resolved ACP agent, and path roots used by the native
    /// structured view, projected from the shared `GET /api/sessions` read.
    pub async fn session_view_info(
        &self,
        session_id: &str,
    ) -> Result<crate::acp::session_paths::SessionViewInfo, HttpError> {
        let envelope = self
            .endpoint
            .daemon_client()?
            .list_sessions(None)
            .await
            .map_err(|error| match error {
                crate::daemon::DaemonClientError::Status { status, .. }
                    if status == StatusCode::UNAUTHORIZED =>
                {
                    HttpError::Unauthorized
                }
                error => HttpError::Daemon(error),
            })?;
        envelope
            .sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .map(crate::acp::session_paths::SessionViewInfo::from)
            .ok_or_else(|| HttpError::SessionNotFound(session_id.to_string()))
    }

    /// Context-window percentage at which the daemon wants the structured
    /// view to show its compaction reminder, or `None` when the reminder is
    /// off. Read from the daemon rather than local config on purpose: the
    /// native view can attach to a remote daemon over `AOE_DAEMON_URL`,
    /// where this host's `config.toml` describes a different install, and
    /// the daemon already resolves the active profile's value for the web
    /// dashboard. See #3253.
    pub async fn compaction_reminder(&self) -> Result<Option<u8>, HttpError> {
        /// The two `/api/about` fields the view needs.
        #[derive(serde::Deserialize)]
        struct ReminderAbout {
            acp_compaction_reminder: bool,
            acp_compaction_reminder_percent: u8,
        }
        let url = format!("{}/api/about", self.endpoint.base_url);
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, "<about>")?;
        let about = crate::daemon::decode_json::<ReminderAbout>(res).await?;
        Ok(about
            .acp_compaction_reminder
            .then_some(about.acp_compaction_reminder_percent)
            .filter(|pct| (1..=99).contains(pct)))
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
        let res = self.execute(self.http.get(&url)).await?;
        check_global_status(res).map(|_| ())
    }

    async fn execute(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, HttpError> {
        let token = self.endpoint.bearer_token();
        let mut request = builder.build()?;
        if let Some(authorization) = crate::daemon::authorization_header(token)? {
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, authorization);
        }
        Ok(
            crate::daemon::transport::execute(&self.http, self.endpoint.unix_path(), request)
                .await?,
        )
    }
}

fn check_status(res: reqwest::Response, session_id: &str) -> Result<reqwest::Response, HttpError> {
    if res.status().is_success() {
        Ok(res)
    } else {
        Err(status_error(&res, Some(session_id), false))
    }
}

fn check_global_status(res: reqwest::Response) -> Result<reqwest::Response, HttpError> {
    if res.status().is_success() {
        Ok(res)
    } else {
        Err(status_error(&res, None, false))
    }
}

fn check_resolve_status(res: reqwest::Response, session_id: &str) -> Result<(), HttpError> {
    if res.status().is_success() {
        Ok(())
    } else {
        Err(status_error(&res, Some(session_id), true))
    }
}

fn status_error(res: &reqwest::Response, session_id: Option<&str>, resolving: bool) -> HttpError {
    use crate::daemon::ApiErrorCode;
    let status = res.status();
    let code = ApiErrorCode::from_headers(status, res.headers(), resolving);
    match (status, code) {
        (StatusCode::UNAUTHORIZED, _) => HttpError::Unauthorized,
        (_, Some(ApiErrorCode::ReadOnly)) => HttpError::ReadOnly,
        (_, Some(ApiErrorCode::PendingTargetGone)) => HttpError::ApprovalGone,
        (StatusCode::NOT_FOUND, _) if session_id.is_some() && !resolving => {
            HttpError::SessionNotFound(session_id.unwrap().to_owned())
        }
        _ => HttpError::Server {
            status,
            body: code.map_or_else(String::new, |code| code.as_str().to_owned()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;
    use std::time::Duration;

    fn endpoint(base: &str, token: Option<&str>) -> DaemonEndpoint {
        DaemonEndpoint::new(base.to_string(), token.map(str::to_string), Source::Env)
    }

    #[tokio::test]
    async fn authenticated_refusal_does_not_read_a_stalled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0, "the client must send its request");
            stream.write_all(b"HTTP/1.1 403 Forbidden\r\nAoE-Error-Code: read_only\r\nContent-Length: 100\r\n\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });
        let client =
            HttpClient::new(endpoint(&format!("http://{address}"), Some("secret"))).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), client.health_check()).await;
        server.abort();
        assert!(matches!(result.unwrap(), Err(HttpError::ReadOnly)));
    }

    #[tokio::test]
    async fn identifiers_cannot_change_the_queue_route() {
        use axum::{extract::Path, routing::patch, Json, Router};
        let app = Router::new().route(
            "/api/sessions/{session}/queue/{prompt}",
            patch(
                |Path((session, prompt)): Path<(String, String)>,
                 Json(body): Json<serde_json::Value>| async move {
                    if session == "session/child"
                        && prompt == "prompt?name#fragment%value"
                        && body["text"] == "replace"
                    {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::UNPROCESSABLE_ENTITY
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client =
            HttpClient::new(endpoint(&format!("http://{address}"), Some("secret"))).unwrap();
        let edited = client
            .queue_edit("session/child", "prompt?name#fragment%value", "replace")
            .await;
        let traversal = client.queue_edit("..", "prompt", "replace").await;
        server.abort();
        edited.unwrap();
        assert!(matches!(
            traversal,
            Err(HttpError::Daemon(
                crate::daemon::DaemonClientError::InvalidPathSegment
            ))
        ));
    }
}
