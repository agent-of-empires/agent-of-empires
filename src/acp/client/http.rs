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
use super::passphrase_session::{self, PassphraseSessionCache};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::protocol::{
    ApprovalDecisionWire, FilesResponse, PromptRequest, ReplayResponse, ResolveApprovalRequest,
    SwitchAgentRequest, SwitchAgentResponse, SwitchProviderRequest, SwitchProviderResponse,
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

/// `GET` and `PATCH /api/log-level` answers; each leaves out the fields the other owns.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct LogFilterReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    pub current: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reloadable: Option<bool>,
    pub ephemeral: bool,
}

/// Acp daemon HTTP client. Cheap to clone; the underlying
/// `reqwest::Client` is reference-counted.
#[derive(Debug, Clone)]
pub struct HttpClient {
    http: reqwest::Client,
    endpoint: DaemonEndpoint,
    passphrase_session: PassphraseSessionCache,
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
    #[error("reviving the session is required to accept this prompt; retry without --no-revive")]
    RevivalRequired,
    #[error("Auto-name skipped: the session changed or no title could be applied")]
    SmartRenameSkipped,
    #[error("The naming agent failed; check its availability and try again")]
    SmartRenameFailed,
    #[error("Auto-name timed out; other sessions may be naming. Try again shortly")]
    SmartRenameTimeout,
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
        Ok(Self {
            http,
            endpoint,
            passphrase_session: PassphraseSessionCache::default(),
        })
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
            self.endpoint.base_url,
            path_segment(session_id)?,
            since
        );
        let res = self.execute(self.http.get(&url)).await?;
        let res = check_status(res, session_id)?;
        decode_replay(res).await
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
        decode_replay(res).await
    }

    /// Fold replay pages through the first page's snapshot window. Lost pages
    /// are consumed before stopping, so callers can reset using the same data.
    async fn replay_pages(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
        view_rows: bool,
        mut consume: impl FnMut(ReplayResponse),
    ) -> Result<(u64, Option<u64>, bool), HttpError> {
        let mut cursor = since;
        let mut target = None;
        loop {
            let page = if view_rows {
                self.replay_rows_page(session_id, cursor, page_size).await?
            } else {
                self.replay_page(session_id, cursor, page_size).await?
            };
            let cap = *target.get_or_insert(page.highest_seq);
            let (highest_seq, lowest_seq, lost) = (page.highest_seq, page.lowest_seq, page.lost);
            let (next_cursor, has_more) = (page.next_cursor, page.has_more);
            consume(page);
            match next_cursor {
                Some(next) if !lost && has_more && next > cursor && next < cap => {
                    cursor = next;
                }
                _ => return Ok((highest_seq, lowest_seq, lost)),
            }
        }
    }

    /// Accumulate raw frames inside the first page's snapshot window. Events
    /// appended later arrive over the live WS; a retention gap stops replay.
    pub async fn replay_paged(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let mut frames = Vec::new();
        let (highest_seq, lowest_seq, lost) = self
            .replay_pages(session_id, since, page_size, false, |page| {
                frames.extend(page.frames);
            })
            .await?;
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

    /// Fetch one page of server-folded transcript rows through the same
    /// authenticated, path-encoded and deadline-bounded path as raw replay.
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
        decode_replay(res).await
    }

    /// Reconcile server-folded rows by ID across page seams, using the same
    /// snapshot cap and retention-gap handling as raw replay.
    pub async fn replay_rows_paged(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
    ) -> Result<(Vec<crate::acp::transcript::TranscriptRow>, bool), HttpError> {
        let mut rows = Vec::new();
        let (_, _, lost) = self
            .replay_pages(session_id, since, page_size, true, |page| {
                if let Some(page_rows) = page.rows {
                    for row in page_rows {
                        crate::acp::transcript::upsert_transcript_row(&mut rows, row);
                    }
                }
            })
            .await?;
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
            queued_prompt_id: None,
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
            .execute_elevated(|| {
                self.http
                    .post(&url)
                    .json(&serde_json::json!({ "enabled": enabled }))
            })
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
        let res = self.execute_elevated(|| self.http.post(&url)).await?;
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

    /// Wait for the one-shot and canonical title publication.
    pub async fn smart_rename(&self, session_id: &str) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/smart-rename",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let timeout =
            crate::session::smart_rename::RENAME_DEADLINE + std::time::Duration::from_secs(15);
        let res = self.execute(self.http.post(&url).timeout(timeout)).await?;
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

    /// A rejection echoes over the WS as `ConfigOptionSwitchFailed`.
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/config-option",
            self.endpoint.base_url,
            path_segment(session_id)?
        );
        let body = serde_json::json!({ "config_id": config_id, "value": value });
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

    pub async fn switch_provider(
        &self,
        session_id: &str,
        provider: &str,
    ) -> Result<SwitchProviderResponse, HttpError> {
        let url = format!(
            "{}/api/sessions/{}/acp/switch-provider",
            self.endpoint.base_url,
            path_segment(session_id)?,
        );
        let body = SwitchProviderRequest {
            provider: provider.to_owned(),
        };
        let response = self.execute(self.http.post(&url).json(&body)).await?;
        let response = check_status(response, session_id)?;
        Ok(crate::daemon::decode_json::<SwitchProviderResponse>(response).await?)
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

    /// Read sessions with the endpoint's bearer token or passphrase session.
    pub async fn list_sessions(
        &self,
        state: Option<crate::session::SessionScope>,
    ) -> Result<crate::daemon::SessionsEnvelope, HttpError> {
        let had_cached_session = self.endpoint.bearer_token().is_none()
            && self.passphrase_session.get(&self.endpoint).is_some();
        match self.list_sessions_once(state).await {
            Err(HttpError::Unauthorized) if had_cached_session => {
                self.passphrase_session.invalidate(&self.endpoint);
                self.list_sessions_once(state).await
            }
            result => result,
        }
    }

    pub(crate) async fn daemon_client(&self) -> Result<crate::daemon::DaemonClient, HttpError> {
        let base_url = &self.endpoint.base_url;
        let client = if let Some(path) = self.endpoint.unix_path() {
            crate::daemon::DaemonClient::new_unix(path)
        } else if let Some(token) = self.endpoint.bearer_token() {
            crate::daemon::DaemonClient::new(base_url, Some(token))
        } else if self.endpoint.resolved_passphrase().is_some() {
            let session = self.ensure_passphrase_session().await?;
            crate::daemon::DaemonClient::with_session(
                base_url,
                &session.cookie,
                &session.binding_secret,
            )
        } else {
            crate::daemon::DaemonClient::new(base_url, None)
        }?;
        Ok(client)
    }

    async fn list_sessions_once(
        &self,
        state: Option<crate::session::SessionScope>,
    ) -> Result<crate::daemon::SessionsEnvelope, HttpError> {
        let client = self.daemon_client().await?;
        client
            .list_sessions(state)
            .await
            .map_err(|error| match error {
                crate::daemon::DaemonClientError::Status { status, .. }
                    if status == StatusCode::UNAUTHORIZED =>
                {
                    HttpError::Unauthorized
                }
                error => HttpError::Daemon(error),
            })
    }

    /// Title, resolved agent, and path roots, from the shared session list.
    pub async fn session_view_info(
        &self,
        session_id: &str,
    ) -> Result<crate::acp::session_paths::SessionViewInfo, HttpError> {
        let envelope = self.list_sessions(None).await?;
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

    /// The daemon's notice display policy, for the same reason as
    /// [`Self::compaction_reminder`].
    pub async fn notice_policy(&self) -> Result<crate::session::config::NoticePolicy, HttpError> {
        #[derive(serde::Deserialize)]
        struct NoticeAbout {
            acp_notice_display: crate::session::config::NoticeDisplay,
            acp_notice_timeout_secs: u32,
        }
        let url = format!("{}/api/about", self.endpoint.base_url);
        let response = self.execute(self.http.get(&url)).await?;
        let response = check_status(response, "<about>")?;
        let about = crate::daemon::decode_json::<NoticeAbout>(response).await?;
        Ok(crate::session::config::NoticePolicy {
            display: about.acp_notice_display,
            timeout_secs: about.acp_notice_timeout_secs,
        })
    }

    /// `GET /api/log-level`, or `PATCH` it first when `patch` is given.
    pub async fn log_level(
        &self,
        patch: Option<&serde_json::Value>,
    ) -> Result<LogFilterReply, HttpError> {
        let url = format!("{}/api/log-level", self.endpoint.base_url);
        let builder = match patch {
            None => self.http.get(&url),
            Some(body) => self.http.patch(&url).json(body),
        };
        let res = check_global_status(self.execute(builder).await?)?;
        Ok(crate::daemon::decode_json::<LogFilterReply>(res).await?)
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

    /// Build, authenticate and send one request, retrying once after a fresh
    /// passphrase login when a *cached* login session comes back 401 (rotated
    /// passphrase, expired or evicted session), or when a bearer token alone is
    /// refused and a passphrase resolves (a daemon that also requires the second
    /// factor). A first-ever login failure is not retried: the credential itself
    /// is wrong, not merely stale.
    async fn execute(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, HttpError> {
        let request = builder.build()?;
        // Every body here is an in-memory JSON payload, so the retry below
        // can reuse the built request. A body that cannot be cloned (none
        // today) simply keeps the first response.
        let retry = request.try_clone();
        let had_cached_session = self.passphrase_session.get(&self.endpoint).is_some();
        let response = self.send_authenticated(request, false).await?;
        let bearer_only =
            !had_cached_session && self.endpoint.bearer_token().is_some() && self.can_login();
        if response.status() != StatusCode::UNAUTHORIZED || !(had_cached_session || bearer_only) {
            return Ok(response);
        }
        self.passphrase_session.invalidate(&self.endpoint);
        let Some(retry) = retry else {
            return Ok(response);
        };
        self.send_authenticated(retry, true).await
    }

    async fn execute_elevated(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, HttpError> {
        let response = self.execute(build()).await?;
        if self.endpoint.unix_path().is_some()
            || self.passphrase_session.get(&self.endpoint).is_none()
            || crate::daemon::ApiErrorCode::from_headers(
                response.status(),
                response.headers(),
                false,
            ) != Some(crate::daemon::ApiErrorCode::ElevationRequired)
        {
            return Ok(response);
        }
        drop(response);
        passphrase_session::elevate(&self.endpoint, &self.passphrase_session).await?;
        self.execute(build()).await
    }

    async fn send_authenticated(
        &self,
        mut request: reqwest::Request,
        force_session: bool,
    ) -> Result<reqwest::Response, HttpError> {
        self.attach_credential(&mut request, force_session).await?;
        Ok(
            crate::daemon::transport::execute(&self.http, self.endpoint.unix_path(), request)
                .await?,
        )
    }

    /// A passphrase can be used here: never over a Unix socket, whose peer owner
    /// already authorizes the request, and `resolved_passphrase` refuses a plaintext
    /// `http://` non-loopback URL.
    fn can_login(&self) -> bool {
        self.endpoint.unix_path().is_none() && self.endpoint.resolved_passphrase().is_some()
    }

    /// Attach whichever credential is available. A cached passphrase-login session
    /// wins, then a bearer token (`--auth=token`), then a fresh login
    /// (`--auth=passphrase`). `force_session` skips the bearer. Nothing resolving
    /// (`--auth=none`) leaves the request untouched.
    async fn attach_credential(
        &self,
        request: &mut reqwest::Request,
        force_session: bool,
    ) -> Result<(), HttpError> {
        let can_login = self.can_login();
        let cached = can_login && self.passphrase_session.get(&self.endpoint).is_some();
        if !(force_session && can_login || cached) {
            if let Some(authorization) =
                crate::daemon::authorization_header(self.endpoint.bearer_token())?
            {
                request
                    .headers_mut()
                    .insert(header::AUTHORIZATION, authorization);
                return Ok(());
            }
            if !can_login {
                return Ok(());
            }
        }
        let session = self.ensure_passphrase_session().await?;
        request.headers_mut().insert(
            header::COOKIE,
            header::HeaderValue::from_str(&session.cookie).map_err(|_| HttpError::Transport)?,
        );
        request.headers_mut().insert(
            "x-aoe-device-binding",
            header::HeaderValue::from_str(&session.binding_secret)
                .map_err(|_| HttpError::Transport)?,
        );
        Ok(())
    }

    async fn ensure_passphrase_session(
        &self,
    ) -> Result<passphrase_session::PassphraseSession, HttpError> {
        if let Some(session) = self.passphrase_session.get(&self.endpoint) {
            return Ok(session);
        }
        passphrase_session::login(&self.endpoint, &self.passphrase_session).await
    }
}

async fn decode_replay(response: reqwest::Response) -> Result<ReplayResponse, HttpError> {
    let deadline = crate::daemon::transport::response_deadline(&response);
    decode_replay_until(response, deadline).await
}

async fn decode_replay_until(
    response: reqwest::Response,
    deadline: tokio::time::Instant,
) -> Result<ReplayResponse, HttpError> {
    use std::io::{self, BufReader};
    use tokio_util::io::{StreamReader, SyncIoBridge};
    use tokio_util::sync::CancellationToken;

    let cancellation = CancellationToken::new();
    // Dropping the caller must wake a blocking reader, not just detach its task.
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let chunks = futures_util::stream::try_unfold(
        (response, cancellation),
        move |(mut response, cancellation)| async move {
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
                }
                chunk = tokio::time::timeout_at(deadline, response.chunk()) => {
                    chunk.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
                        .map_err(|error| io::Error::from(if error.is_timeout() {
                            io::ErrorKind::TimedOut
                        } else {
                            io::ErrorKind::ConnectionAborted
                        }))?
                }
            };
            Ok(chunk.map(|chunk| (chunk, (response, cancellation))))
        },
    );
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let chunks = std::pin::pin!(chunks);
        let bridge = SyncIoBridge::new_with_handle(StreamReader::new(chunks), runtime);
        serde_json::from_reader(BufReader::with_capacity(64 * 1024, bridge)).map_err(|error| {
            match error.io_error_kind() {
                Some(io::ErrorKind::TimedOut) => {
                    HttpError::Daemon(crate::daemon::DaemonClientError::Timeout)
                }
                Some(_) => HttpError::Transport,
                None => HttpError::Daemon(crate::daemon::DaemonClientError::AuthenticatedDecode),
            }
        })
    })
    .await
    .map_err(|_| HttpError::Transport)?
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
    classify_status(res.status(), res.headers(), session_id, resolving)
}

/// Classify from the status and the one validated `AoE-Error-Code` header, never a body.
pub(super) fn classify_status(
    status: StatusCode,
    headers: &header::HeaderMap,
    session_id: Option<&str>,
    resolving: bool,
) -> HttpError {
    use crate::daemon::ApiErrorCode;
    let code = ApiErrorCode::from_headers(status, headers, resolving);
    match (status, code) {
        (StatusCode::UNAUTHORIZED, _) => HttpError::Unauthorized,
        (_, Some(ApiErrorCode::ReadOnly)) => HttpError::ReadOnly,
        (_, Some(ApiErrorCode::NoRevive)) => HttpError::RevivalRequired,
        (_, Some(ApiErrorCode::PendingTargetGone)) => HttpError::ApprovalGone,
        (_, Some(ApiErrorCode::SmartRenameSkipped)) => HttpError::SmartRenameSkipped,
        (_, Some(ApiErrorCode::SmartRenameFailed)) => HttpError::SmartRenameFailed,
        (_, Some(ApiErrorCode::SmartRenameTimeout)) => HttpError::SmartRenameTimeout,
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
    async fn replay_streams_large_pages_and_individual_tool_rows() {
        use crate::acp::state::{Event, ToolOutputBlock};
        use crate::acp::transcript::TranscriptRowKind;
        use axum::{middleware, response::IntoResponse, routing::get, Router};
        use futures_util::StreamExt;

        let baseline = crate::session::Instance::new("baseline", "/tmp");
        let large = crate::session::Instance::new("large", "/tmp");
        let baseline_id = baseline.id.clone();
        let large_id = large.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![baseline, large]);
        let text = format!("{}end-🦀", "x".repeat(384 * 1024));
        for seq in 1..=64 {
            state
                .acp_event_store
                .record(
                    &baseline_id,
                    seq,
                    &Event::AgentMessageChunk { text: text.clone() },
                )
                .unwrap();
        }
        state
            .acp_event_store
            .record(
                &baseline_id,
                65,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        let content = format!("{}tool-tail-🦀", "y".repeat(17 * 1024 * 1024));
        let output = vec![ToolOutputBlock::Text {
            text: "structured output".into(),
        }];
        let completed_at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        state
            .acp_event_store
            .record(
                &large_id,
                1,
                &Event::ToolCallCompleted {
                    tool_call_id: "tool".into(),
                    is_error: false,
                    content: content.clone(),
                    output: output.clone(),
                    completed_at,
                    async_subagent: false,
                },
            )
            .unwrap();

        for chunked in [false, true] {
            let mut app = Router::new()
                .route(
                    "/api/sessions/{id}/acp/replay",
                    get(crate::server::api::acp_replay),
                )
                .with_state(state.clone());
            if chunked {
                app = app.layer(middleware::map_response(
                    |response: axum::response::Response| async {
                        let (mut parts, body) = response.into_parts();
                        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                        let chunks = body.into_data_stream().flat_map(|chunk| {
                            let chunks: Vec<_> = match chunk {
                                Ok(bytes) => (0..bytes.len())
                                    .step_by(16 * 1024)
                                    .map(|start| {
                                        Ok(bytes.slice(start..(start + 16 * 1024).min(bytes.len())))
                                    })
                                    .collect(),
                                Err(error) => vec![Err(error)],
                            };
                            futures_util::stream::iter(chunks)
                        });
                        axum::response::Response::from_parts(
                            parts,
                            axum::body::Body::from_stream(chunks),
                        )
                        .into_response()
                    },
                ));
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client =
                HttpClient::new(endpoint(&format!("http://{address}"), Some("secret"))).unwrap();

            let probe = client.replay(&baseline_id, 0).await.unwrap();
            assert_eq!(probe.frames.len(), 65);
            assert_eq!(probe.highest_seq, 65);
            drop(probe);
            let replay = client.replay_paged(&baseline_id, 0, 64).await.unwrap();
            assert!(!replay.lost);
            assert_eq!(replay.lowest_seq, Some(1));
            assert_eq!(replay.highest_seq, 65);
            assert_eq!(replay.frames.len(), 65);
            for (index, frame) in replay.frames[..64].iter().enumerate() {
                assert_eq!(frame.session_id, baseline_id);
                assert_eq!(frame.seq, index as u64 + 1);
                let Event::AgentMessageChunk { text: actual } = &*frame.event else {
                    panic!("expected message frame");
                };
                assert_eq!(actual, &text);
            }
            assert_eq!(replay.frames[64].seq, 65);
            assert!(
                matches!(&*replay.frames[64].event, Event::Stopped { reason } if reason == "prompt_complete")
            );
            drop(replay);
            let (rows, lost) = client.replay_rows_paged(&baseline_id, 0, 64).await.unwrap();
            assert!(!lost);
            assert_eq!(rows.len(), 64);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(row.id, format!("msg-{}", index + 1));
                assert_eq!(row.text, text);
            }

            drop(rows);
            let replay = client.replay_page(&large_id, 0, 1).await.unwrap();
            assert_eq!(replay.frames.len(), 1);
            assert_eq!(replay.frames[0].seq, 1);
            assert_eq!(replay.next_cursor, Some(1));
            let Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                content: actual,
                output: actual_output,
                completed_at: actual_time,
                async_subagent,
            } = &*replay.frames[0].event
            else {
                panic!("expected completed tool");
            };
            assert_eq!(tool_call_id, "tool");
            assert!(!is_error);
            assert_eq!(actual, &content);
            assert_eq!(actual_output, &output);
            assert_eq!(*actual_time, completed_at);
            assert!(!async_subagent);
            drop(replay);
            let (rows, lost) = client.replay_rows_paged(&large_id, 0, 1).await.unwrap();
            assert!(!lost);
            let row = rows
                .iter()
                .find(|row| row.kind == TranscriptRowKind::ToolComplete)
                .unwrap();
            assert_eq!(row.text, content);
            assert_eq!(row.output, output);
            assert_eq!(row.tool_call_id.as_deref(), Some("tool"));
            assert_eq!(row.at, completed_at);

            drop(rows);
            let response = client
                .execute(client.http.get(format!(
                    "http://{address}/api/sessions/{baseline_id}/acp/replay?since=0&limit=64"
                )))
                .await
                .unwrap();
            if chunked {
                assert_eq!(response.content_length(), None);
            } else {
                assert!(response.content_length().unwrap() > 16 * 1024 * 1024);
            }
            let error = crate::daemon::decode_json::<ReplayResponse>(response)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                crate::daemon::DaemonClientError::ResponseTooLarge { .. }
            ));
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn replay_rejects_incomplete_or_extra_documents_without_reflecting_payloads() {
        let valid = r#"{"frames":[],"lost":false,"highest_seq":9,"next_cursor":9}"#;
        for body in [
            valid[..valid.len() - 1].to_owned(),
            format!("{valid} secret-token"),
            format!("{valid}{valid}"),
            r#"{"frames":[],"lost":"secret-token","highest_seq":9}"#.to_owned(),
        ] {
            let response = reqwest::Response::from(axum::http::Response::new(body));
            let error = decode_replay(response).await.unwrap_err();
            assert!(matches!(
                error,
                HttpError::Daemon(crate::daemon::DaemonClientError::AuthenticatedDecode)
            ));
            assert!(!error.to_string().contains("secret-token"));
        }
        let response = reqwest::Response::from(axum::http::Response::new(format!("{valid} \n\t")));
        let replay = decode_replay(response).await.unwrap();
        assert_eq!(replay.highest_seq, 9);
        assert_eq!(replay.next_cursor, Some(9));
    }

    struct ObservedBody {
        prefix: Option<axum::body::Bytes>,
        waiting: Option<tokio::sync::oneshot::Sender<()>>,
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl Drop for ObservedBody {
        fn drop(&mut self) {
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    fn stalled_replay_body(
        prefix: &'static str,
    ) -> (
        reqwest::Response,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (waiting_tx, waiting) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped) = tokio::sync::oneshot::channel();
        let mut body = ObservedBody {
            prefix: Some(axum::body::Bytes::from_static(prefix.as_bytes())),
            waiting: Some(waiting_tx),
            dropped: Some(dropped_tx),
        };
        let stream = futures_util::stream::poll_fn(move |_| {
            if let Some(prefix) = body.prefix.take() {
                return std::task::Poll::Ready(Some(Ok::<_, std::io::Error>(prefix)));
            }
            if let Some(waiting) = body.waiting.take() {
                let _ = waiting.send(());
            }
            std::task::Poll::Pending
        });
        (
            reqwest::Response::from(axum::http::Response::new(reqwest::Body::wrap_stream(
                stream,
            ))),
            waiting,
            dropped,
        )
    }

    #[tokio::test]
    async fn replay_body_deadline_covers_incomplete_json_and_trailing_whitespace() {
        for prefix in [
            r#"{"frames":["#,
            "{\"frames\":[],\"lost\":false,\"highest_seq\":9} \n\t",
        ] {
            let (response, waiting, dropped) = stalled_replay_body(prefix);
            let decoder = tokio::spawn(decode_replay_until(
                response,
                tokio::time::Instant::now() + Duration::from_secs(1),
            ));
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(2), decoder)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                error,
                HttpError::Daemon(crate::daemon::DaemonClientError::Timeout)
            ));
            tokio::time::timeout(Duration::from_secs(2), dropped)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn cancelling_replay_releases_the_blocking_body_reader() {
        let (response, waiting, dropped) = stalled_replay_body(r#"{"frames":["#);
        let decoder = tokio::spawn(decode_replay(response));
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        decoder.abort();
        assert!(decoder.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), dropped)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn replay_body_transport_errors_are_redacted() {
        let stream = futures_util::stream::iter([
            Ok(axum::body::Bytes::from_static(br#"{"frames":["#)),
            Err(std::io::Error::other("secret-token")),
        ]);
        let response = reqwest::Response::from(axum::http::Response::new(
            reqwest::Body::wrap_stream(stream),
        ));
        let error = decode_replay(response).await.unwrap_err();
        assert!(matches!(error, HttpError::Transport));
        assert!(!error.to_string().contains("secret-token"));
    }

    async fn serve_stalled_replay<S>(mut stream: S, prefix: &'static str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buf).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
        }
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{prefix}",
                    prefix.len() + 64,
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(
            stream.read(&mut buf).await.unwrap(),
            0,
            "decoder must close its stalled body"
        );
    }

    #[tokio::test]
    async fn replay_body_deadline_applies_to_tcp_and_unix_transports() {
        for unix in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (endpoint, server) = if unix {
                let path = dir.path().join("api.sock");
                let listener = tokio::net::UnixListener::bind(&path).unwrap();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    serve_stalled_replay(stream, r#"{"frames":["#).await;
                });
                (DaemonEndpoint::local_unix(path), server)
            } else {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    serve_stalled_replay(stream, r#"{"frames":["#).await;
                });
                (
                    endpoint(&format!("http://{address}"), Some("secret")),
                    server,
                )
            };
            let client = HttpClient::new(endpoint).unwrap();
            let response = client
                .execute(
                    client
                        .http
                        .get(format!(
                            "{}/api/sessions/test/acp/replay?since=0",
                            client.endpoint.base_url
                        ))
                        .timeout(Duration::from_millis(100)),
                )
                .await
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(2), decode_replay(response))
                .await
                .expect("the original request deadline bounds replay");
            assert!(matches!(
                result,
                Err(HttpError::Daemon(crate::daemon::DaemonClientError::Timeout))
            ));
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
        }
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
    async fn smart_rename_reports_fixed_errors_without_reflecting_daemon_content() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, code) in [
            (409, "smart_rename_skipped"),
            (502, "smart_rename_failed"),
            (504, "smart_rename_timeout"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let headers = format!(
                    "HTTP/1.1 {status} Error\r\nAoE-Error-Code: {code}\r\nContent-Length: 100\r\n\r\n"
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream
                    .write_all(b"secret-provider-credential")
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            });
            let client =
                HttpClient::new(endpoint(&format!("http://{address}"), Some("secret"))).unwrap();
            let result =
                tokio::time::timeout(Duration::from_secs(1), client.smart_rename("session")).await;
            server.abort();
            let error = result.unwrap().unwrap_err();
            assert!(matches!(
                (status, &error),
                (409, HttpError::SmartRenameSkipped)
                    | (502, HttpError::SmartRenameFailed)
                    | (504, HttpError::SmartRenameTimeout)
            ));
            assert!(!error.to_string().contains("secret-provider-credential"));
        }
    }

    /// The request `attach_credential` would put on the wire, headers and all.
    async fn credentialed(client: &HttpClient, url: &str) -> reqwest::Request {
        let mut request = client.http.get(url).build().unwrap();
        client.attach_credential(&mut request, false).await.unwrap();
        request
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_credential_without_token_or_passphrase() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let client = HttpClient::new(endpoint("http://127.0.0.1:8080", None)).unwrap();
        let request = credentialed(&client, "http://127.0.0.1:8080/api/sessions").await;
        assert!(request.headers().get(header::AUTHORIZATION).is_none());
        assert!(request.headers().get(header::COOKIE).is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_bearer_token_wins_over_a_passphrase() {
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", "hunter2")]);
        let client = HttpClient::new(endpoint("http://127.0.0.1:8080", Some("tok"))).unwrap();

        let request = credentialed(&client, "http://127.0.0.1:8080/api/sessions").await;
        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer tok"
        );
        assert!(request.headers().get(header::COOKIE).is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_cached_passphrase_session_rides_as_a_cookie() {
        let _app = crate::session::test_support::isolate_app_dir();
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", "hunter2")]);
        let client = HttpClient::new(endpoint("http://127.0.0.1:8080", None)).unwrap();
        client
            .passphrase_session
            .set_for_test(passphrase_session::PassphraseSession {
                cookie: "aoe_session=abc123".to_string(),
                binding_secret: "the-binding-secret".to_string(),
            });

        let request = credentialed(&client, "http://127.0.0.1:8080/api/sessions").await;
        assert!(request.headers().get(header::AUTHORIZATION).is_none());
        assert_eq!(
            request.headers().get(header::COOKIE).unwrap(),
            "aoe_session=abc123"
        );
        assert_eq!(
            request.headers().get("X-Aoe-Device-Binding").unwrap(),
            "the-binding-secret"
        );
    }

    #[tokio::test]
    async fn no_revive_refusal_explains_the_conflict_without_reading_its_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream.write_all(b"HTTP/1.1 409 Conflict\r\nAoE-Error-Code: no_revive\r\nContent-Length: 100\r\n\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });
        let client =
            HttpClient::new(endpoint(&format!("http://{address}"), Some("secret"))).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.prompt("session", "hello", true),
        )
        .await;
        server.abort();
        let error = result
            .expect("classification must not wait for the stalled body")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("reviving the session is required"),
            "{error}"
        );
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

    #[tokio::test]
    #[serial_test::serial]
    async fn log_level_reads_and_patches_without_surfacing_an_error_body() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        use axum::{response::IntoResponse, routing::get, Json, Router};
        let app = Router::new().route(
            "/api/log-level",
            get(|| async {
                Json(serde_json::json!({
                    "current": "info", "reloadable": true, "ephemeral": true
                }))
            })
            .patch(|Json(body): Json<serde_json::Value>| async move {
                if body["filter"] == "bad" {
                    return (StatusCode::BAD_REQUEST, "secret-token echoed").into_response();
                }
                Json(serde_json::json!({
                    "previous": "info", "current": "debug", "ephemeral": true
                }))
                .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = HttpClient::new(endpoint(&format!("http://{address}"), None)).unwrap();
        let read = client.log_level(None).await.unwrap();
        let patched = client
            .log_level(Some(&serde_json::json!({ "level": "debug" })))
            .await
            .unwrap();
        let rejected = client
            .log_level(Some(&serde_json::json!({ "filter": "bad" })))
            .await
            .unwrap_err();
        server.abort();
        assert_eq!(read.reloadable, Some(true));
        assert_eq!(patched.previous.as_deref(), Some("info"));
        assert!(!rejected.to_string().contains("secret-token"));
    }

    /// Against the real router, a bearer alone is refused on a token-plus-passphrase
    /// daemon, and the fallback login must itself carry the bearer.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_bearer_falls_back_to_a_passphrase_login_on_a_real_router() {
        use crate::server::test_support;
        let mut _env = test_support::RuntimeEnvGuard::read_lock();
        let dir = tempfile::tempdir().unwrap();
        _env.bind(dir.path());
        let phrase = "http client second factor";
        let token = "b".repeat(64);
        let _phrase =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", phrase)]);
        let state = test_support::build_test_app_state_with_policy_configured(
            Vec::new(),
            vec!["127.0.0.1".into()],
            Vec::new(),
            Some(token.clone()),
            |state| {
                state.login_manager =
                    std::sync::Arc::new(crate::server::login::LoginManager::new(Some(phrase)));
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = test_support::build_router_for_test(state.clone());
        let shutdown = state.shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        let client = HttpClient::new(endpoint(&base, Some(&token))).unwrap();
        client.health_check().await.unwrap();
        client.health_check().await.unwrap();
        assert_eq!(
            state.login_manager.device_snapshot(None).await.len(),
            1,
            "one login, then the cached session"
        );
        state.shutdown.cancel();
        server.await.unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn session_list_logs_in_renews_once_and_fails_clearly() {
        use super::passphrase_session::test_daemon::{TestDaemon, PASSPHRASE};
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);

        let daemon = TestDaemon::start(PASSPHRASE).await;
        let client = HttpClient::new(daemon.endpoint.clone()).unwrap();

        let info = client.session_view_info("s-1").await.unwrap();
        assert_eq!(info.title, "one");
        assert_eq!(daemon.logins(), 1);
        let requests = daemon.list_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .0
            .as_deref()
            .is_some_and(|c| c.starts_with("aoe_session=")));
        assert!(requests[0].1.is_some());

        // Cached session is reused without another login.
        client.list_sessions(None).await.unwrap();
        assert_eq!(daemon.logins(), 1);

        // Expired session: one re-login, one retry.
        daemon.expire_session();
        client.list_sessions(None).await.unwrap();
        assert_eq!(daemon.logins(), 2);

        // Wrong passphrase: a single login attempt and a clear 401.
        let wrong = TestDaemon::start("other").await;
        let client = HttpClient::new(wrong.endpoint.clone()).unwrap();
        let error = client.list_sessions(None).await.unwrap_err();
        assert!(matches!(error, HttpError::Unauthorized), "{error}");
        assert_eq!(wrong.logins(), 1);
        assert!(!error.to_string().contains(PASSPHRASE));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn set_config_option_posts_the_pick_with_passphrase_credentials() {
        use super::passphrase_session::test_daemon::{TestDaemon, PASSPHRASE};
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);

        let daemon = TestDaemon::start(PASSPHRASE).await;
        let client = HttpClient::new(daemon.endpoint.clone()).unwrap();
        client
            .set_config_option("s-1", "reasoning_effort", "high")
            .await
            .unwrap();

        let requests = daemon.config_option_requests();
        assert_eq!(requests.len(), 1);
        let (id, cookie, binding, body) = &requests[0];
        assert_eq!(id, "s-1");
        assert!(cookie
            .as_deref()
            .is_some_and(|c| c.starts_with("aoe_session=")));
        assert!(binding.is_some());
        assert_eq!(
            body,
            &serde_json::json!({"config_id": "reasoning_effort", "value": "high"})
        );
    }
}
