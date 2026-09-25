//! HTTP client for the daemon's structured view REST surface. The token is
//! always sent as a bearer header, never in the URL.

use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use reqwest::{header, Method, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;

use super::discovery::DaemonEndpoint;
use super::passphrase_session::{self, PassphraseSessionCache};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::protocol::{
    ApprovalDecisionWire, FilesResponse, PromptRequest, ReplayResponse, ResolveApprovalRequest,
    SwitchAgentRequest, SwitchAgentResponse,
};
use crate::acp::transcript::TranscriptRow;
use crate::plugin::ui_state::UiSnapshot;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Escapes bytes that could break out of one URL path segment while leaving a
/// dotted plugin fqid intact.
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

/// An active plugin command as the daemon reports it. The structured view
/// resolves keybinds against this because a remote daemon's plugins may not
/// exist locally.
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
    /// An empty body from an older daemon means `sent`.
    #[serde(default)]
    pub disposition: PromptDispositionWire,
    /// Present only on `queued`.
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

/// At or under the server's `MAX_REPLAY_PAGE`, so it is never clamped.
pub const REPLAY_PAGE_SIZE: u64 = 1000;

#[derive(Debug, Clone)]
pub struct HttpClient {
    http: reqwest::Client,
    endpoint: DaemonEndpoint,
    passphrase_session: PassphraseSessionCache,
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("structured view session {0} not found on the daemon")]
    SessionNotFound(String),
    /// The approval or elicitation already resolved server-side, so the card
    /// can clear quietly (#1821).
    #[error("approval already resolved")]
    ApprovalGone,
    #[error("daemon is read-only (started with --read-only); request refused")]
    ReadOnly,
    /// Worded for every auth mode, not only token auth (#1525).
    #[error("daemon rejected the request (401); restart `aoe serve` or check `--auth` mode")]
    Unauthorized,
    #[error("daemon returned HTTP {status}: {body}")]
    Server { status: StatusCode, body: String },
    #[error(transparent)]
    Daemon(#[from] crate::daemon::DaemonClientError),
}

/// Which endpoint family a request belongs to, deciding what a 404 means.
#[derive(Clone, Copy)]
enum Scope<'a> {
    /// A 404 is a missing session.
    Session(&'a str),
    /// Daemon-wide: a 404 is an absent route (older daemon).
    Global,
}

impl HttpClient {
    pub fn new(endpoint: DaemonEndpoint) -> Result<Self, HttpError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(concat!("aoe-acp-client/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            endpoint,
            passphrase_session: PassphraseSessionCache::default(),
        })
    }

    /// Bare, unauthenticated request: callers rebuild this on every attempt
    /// (see `send`), so it stays cheap data (method + path), not a
    /// once-usable `RequestBuilder`.
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.endpoint.base_url);
        self.http.request(method, url)
    }

    /// Attach whichever credential is available via `send_authed` (retrying
    /// once on a stale cached passphrase session), then classify the
    /// response by `scope`.
    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        scope: Scope<'_>,
    ) -> Result<reqwest::Response, HttpError> {
        let res = self.send_authed(build).await?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body = res.text().await.unwrap_or_default();
        Err(classify_error(status, &body, scope))
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        scope: Scope<'_>,
    ) -> Result<T, HttpError> {
        let res = self.send(|| self.request(Method::GET, path), scope).await?;
        Ok(res.json().await?)
    }

    /// POST/PATCH/DELETE to a session endpoint, discarding the body.
    async fn session_call(
        &self,
        method: Method,
        session_id: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(), HttpError> {
        let full_path = format!("/api/sessions/{session_id}{path}");
        let build = || {
            let request = self.request(method.clone(), &full_path);
            match &body {
                Some(body) => request.json(body),
                None => request,
            }
        };
        self.send(build, Scope::Session(session_id)).await?;
        Ok(())
    }

    /// One unbounded page (the server's default bound applies). The status
    /// probe uses `since=u64::MAX` for metadata only; history consumers use
    /// [`replay_paged`](Self::replay_paged).
    pub async fn replay(&self, session_id: &str, since: u64) -> Result<ReplayResponse, HttpError> {
        let path = format!("/api/sessions/{session_id}/acp/replay?since={since}");
        self.get_json(&path, Scope::Session(session_id)).await
    }

    pub async fn replay_page(
        &self,
        session_id: &str,
        since: u64,
        limit: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let path = format!("/api/sessions/{session_id}/acp/replay?since={since}&limit={limit}");
        self.get_json(&path, Scope::Session(session_id)).await
    }

    /// Fetch bounded pages from `since`, capped at the first page's
    /// `highest_seq` (later events arrive over the WS), stopping at a
    /// retention gap. Returns the pages and whether a gap was reported.
    async fn collect_pages(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
        view_rows: bool,
    ) -> Result<(Vec<ReplayResponse>, bool), HttpError> {
        let view = if view_rows { "&view=rows" } else { "" };
        let mut pages = Vec::new();
        let mut cursor = since;
        let mut cap = None;
        loop {
            let path = format!(
                "/api/sessions/{session_id}/acp/replay?since={cursor}&limit={page_size}{view}"
            );
            let page: ReplayResponse = self.get_json(&path, Scope::Session(session_id)).await?;
            let cap = *cap.get_or_insert(page.highest_seq);
            let (lost, next, has_more) = (page.lost, page.next_cursor, page.has_more);
            pages.push(page);
            if lost {
                return Ok((pages, true));
            }
            match next {
                Some(next) if has_more && next > cursor && next < cap => cursor = next,
                _ => return Ok((pages, false)),
            }
        }
    }

    /// Every frame from `since` in one response; see `collect_pages`.
    pub async fn replay_paged(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
    ) -> Result<ReplayResponse, HttpError> {
        let (pages, lost) = self
            .collect_pages(session_id, since, page_size, false)
            .await?;
        let last = pages.last().expect("at least one page");
        let (highest_seq, lowest_seq) = (last.highest_seq, last.lowest_seq);
        Ok(ReplayResponse {
            frames: pages.into_iter().flat_map(|page| page.frames).collect(),
            lost,
            highest_seq,
            lowest_seq,
            next_cursor: None,
            has_more: false,
            rows: None,
        })
    }

    /// Server-folded transcript rows from `since`. Pages fold in isolation, so
    /// rows are reconciled by id across page seams.
    pub async fn replay_rows_paged(
        &self,
        session_id: &str,
        since: u64,
        page_size: u64,
    ) -> Result<(Vec<TranscriptRow>, bool), HttpError> {
        let (pages, lost) = self
            .collect_pages(session_id, since, page_size, true)
            .await?;
        let mut rows = Vec::new();
        for row in pages
            .into_iter()
            .flat_map(|page| page.rows.unwrap_or_default())
        {
            crate::acp::transcript::upsert_transcript_row(&mut rows, row);
        }
        Ok((rows, lost))
    }

    /// Workspace files for the composer's `@`-mention picker.
    pub async fn files(&self, session_id: &str) -> Result<FilesResponse, HttpError> {
        let path = format!("/api/sessions/{session_id}/acp/files");
        self.get_json(&path, Scope::Session(session_id)).await
    }

    /// Returns whether the daemon sent, steered, or queued the prompt.
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        no_revive: bool,
    ) -> Result<PromptDispatchWire, HttpError> {
        let body = PromptRequest {
            text: text.to_string(),
            attachments: Vec::new(),
            prompt_id: None,
            no_revive,
        };
        let path = format!("/api/sessions/{session_id}/acp/prompt");
        let res = self
            .send(
                || self.request(Method::POST, &path).json(&body),
                Scope::Session(session_id),
            )
            .await?;
        Ok(res.json::<PromptDispatchWire>().await.unwrap_or_default())
    }

    /// The daemon-wide plugin UI snapshot (#2402).
    pub async fn plugin_ui_state(&self) -> Result<UiSnapshot, HttpError> {
        self.get_json("/api/plugins/ui-state", Scope::Global).await
    }

    pub async fn plugin_commands(&self) -> Result<Vec<PluginCommandView>, HttpError> {
        let envelope: PluginCommandsEnvelope = self
            .get_json("/api/plugins/commands", Scope::Global)
            .await?;
        Ok(envelope.commands)
    }

    /// Fire-and-forget dispatch of an action-less plugin command.
    pub async fn invoke_plugin_command(
        &self,
        fqid: &str,
        session_id: &str,
    ) -> Result<(), HttpError> {
        let path = format!(
            "/api/plugins/commands/{}/invoke",
            utf8_percent_encode(fqid, PATH_SEGMENT)
        );
        let body = serde_json::json!({ "session_id": session_id });
        self.send(
            || self.request(Method::POST, &path).json(&body),
            Scope::Global,
        )
        .await?;
        Ok(())
    }

    /// Toggled through the daemon so its plugin host reconciles workers live.
    pub async fn set_plugin_enabled(
        &self,
        plugin_id: &str,
        enabled: bool,
    ) -> Result<(), HttpError> {
        let path = format!("/api/plugins/{plugin_id}/enabled");
        let body = serde_json::json!({ "enabled": enabled });
        self.send(
            || self.request(Method::POST, &path).json(&body),
            Scope::Global,
        )
        .await?;
        Ok(())
    }

    /// Reload plugins from disk and replace this plugin's worker.
    pub async fn restart_plugin_worker(&self, plugin_id: &str) -> Result<(), HttpError> {
        let path = format!("/api/plugins/{plugin_id}/worker/restart");
        self.send(|| self.request(Method::POST, &path), Scope::Global)
            .await?;
        Ok(())
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), HttpError> {
        self.session_call(Method::POST, session_id, "/acp/cancel", None)
            .await
    }

    /// The daemon-owned prompt queue, ordered by `seq`. The native view never
    /// enqueues itself: the daemon parks prompts posted to `/acp/prompt`.
    pub async fn queue_list(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::daemon::QueuedPromptEntry>, HttpError> {
        let path = format!("/api/sessions/{session_id}/queue");
        self.get_json(&path, Scope::Session(session_id)).await
    }

    /// Replace a queued prompt's text in place. The client-minted id is
    /// encoded as one path segment.
    pub async fn queue_edit(
        &self,
        session_id: &str,
        prompt_id: &str,
        text: &str,
    ) -> Result<(), HttpError> {
        let path = format!("/queue/{}", utf8_percent_encode(prompt_id, PATH_SEGMENT));
        let body = serde_json::json!({ "text": text });
        self.session_call(Method::PATCH, session_id, &path, Some(body))
            .await
    }

    pub async fn queue_clear(&self, session_id: &str) -> Result<(), HttpError> {
        self.session_call(Method::DELETE, session_id, "/queue", None)
            .await
    }

    /// "Auto-name now": 2xx means started; the title arrives over the WS.
    pub async fn smart_rename(&self, session_id: &str) -> Result<(), HttpError> {
        self.session_call(Method::POST, session_id, "/smart-rename", None)
            .await
    }

    /// The result echoes over the WS as `CurrentModeChanged` or
    /// `ModeSwitchFailed`.
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), HttpError> {
        let body = serde_json::json!({ "mode_id": mode_id });
        self.session_call(Method::POST, session_id, "/acp/mode", Some(body))
            .await
    }

    /// Switch a terminal session to the structured view. Idempotent.
    pub async fn acp_enable(&self, session_id: &str) -> Result<(), HttpError> {
        self.session_call(Method::POST, session_id, "/acp/enable", None)
            .await
    }

    /// Switch a structured session back to a terminal. Idempotent.
    pub async fn acp_disable(&self, session_id: &str) -> Result<(), HttpError> {
        self.session_call(Method::POST, session_id, "/acp/disable", None)
            .await
    }

    /// Hand the session to another ACP backend, keeping the transcript.
    pub async fn switch_agent(
        &self,
        session_id: &str,
        target: &str,
        model: Option<&str>,
        reason: Option<&str>,
    ) -> Result<SwitchAgentResponse, HttpError> {
        let body = SwitchAgentRequest {
            target: target.to_string(),
            model: model.map(str::to_string),
            reason: reason.map(str::to_string),
        };
        let path = format!("/api/sessions/{session_id}/acp/switch-agent");
        let res = self
            .send(
                || self.request(Method::POST, &path).json(&body),
                Scope::Session(session_id),
            )
            .await?;
        Ok(res.json().await?)
    }

    async fn resolve(
        &self,
        session_id: &str,
        kind: &str,
        nonce: &str,
        body: &impl serde::Serialize,
    ) -> Result<(), HttpError> {
        let path = format!("/api/sessions/{session_id}/acp/{kind}/{nonce}");
        let res = self
            .send_authed(|| self.request(Method::POST, &path).json(body))
            .await?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let text = res.text().await.unwrap_or_default();
        Err(classify_resolve_error(status, &text, nonce, session_id))
    }

    /// `option_id` names an option chosen through the option picker.
    pub async fn resolve_approval(
        &self,
        session_id: &str,
        nonce: &str,
        decision: ApprovalDecisionWire,
        option_id: Option<String>,
    ) -> Result<(), HttpError> {
        let body = ResolveApprovalRequest {
            decision,
            option_id,
        };
        self.resolve(session_id, "approvals", nonce, &body).await
    }

    pub async fn resolve_elicitation(
        &self,
        session_id: &str,
        nonce: &str,
        resolution: &ElicitationResolution,
    ) -> Result<(), HttpError> {
        self.resolve(session_id, "elicitations", nonce, resolution)
            .await
    }

    /// Title, resolved agent, and path roots, from the shared session list.
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

    /// The compaction reminder percentage, or `None` when off. Read from the
    /// daemon because a remote daemon's config differs from this host's (#3253).
    pub async fn compaction_reminder(&self) -> Result<Option<u8>, HttpError> {
        #[derive(serde::Deserialize)]
        struct ReminderAbout {
            acp_compaction_reminder: bool,
            acp_compaction_reminder_percent: u8,
        }
        let about: ReminderAbout = self
            .get_json("/api/about", Scope::Session("<about>"))
            .await?;
        Ok(about
            .acp_compaction_reminder
            .then_some(about.acp_compaction_reminder_percent)
            .filter(|pct| (1..=99).contains(pct)))
    }

    /// Cheapest authenticated probe: separates a down host (transport error)
    /// from misconfigured auth (401).
    pub async fn health_check(&self) -> Result<(), HttpError> {
        let res = self
            .send_authed(|| self.request(Method::GET, "/api/sessions"))
            .await?;
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
        match self.endpoint.resolved_token() {
            Some(token) => builder.header(header::AUTHORIZATION, format!("Bearer {token}")),
            None => builder,
        }
    }

    /// Attach whichever credential is available: a bearer token when one
    /// resolves (`--auth=token`, unchanged), otherwise the passphrase-login
    /// cookie (`--auth=passphrase`), logging in on first use. Neither
    /// resolving (`--auth=none`, or no daemon credential at all) leaves the
    /// request untouched.
    async fn authed_builder(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        if self.endpoint.resolved_token().is_some() {
            return Ok(self.auth(builder));
        }
        if self.endpoint.resolved_passphrase().is_some() {
            let session = self.ensure_passphrase_session().await?;
            return Ok(builder
                .header(header::COOKIE, session.cookie)
                .header("X-Aoe-Device-Binding", session.binding_secret));
        }
        Ok(builder)
    }

    async fn ensure_passphrase_session(
        &self,
    ) -> Result<passphrase_session::PassphraseSession, HttpError> {
        if let Some(session) = self.passphrase_session.get(&self.endpoint) {
            return Ok(session);
        }
        passphrase_session::login(&self.endpoint, &self.passphrase_session).await
    }

    /// Build, authenticate, and send a request via `build`, retrying once
    /// after a fresh passphrase login if a *cached* session came back 401
    /// (rotated passphrase, expired or evicted session). A first-ever login
    /// failure, or a 401 with a bearer token, is not retried: the credential
    /// itself is wrong, not merely stale.
    async fn send_authed(
        &self,
        mut build: impl FnMut() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, HttpError> {
        let had_cached_session = self.passphrase_session.get(&self.endpoint).is_some();
        let request = self.authed_builder(build()).await?;
        let response = request.send().await?;
        if !had_cached_session || response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        self.passphrase_session.invalidate(&self.endpoint);
        let request = self.authed_builder(build()).await?;
        Ok(request.send().await?)
    }
}

fn classify_error(status: StatusCode, body: &str, scope: Scope<'_>) -> HttpError {
    match (status, scope) {
        (StatusCode::UNAUTHORIZED, _) => HttpError::Unauthorized,
        (StatusCode::FORBIDDEN, _) if body.contains("read-only") || body.contains("read_only") => {
            HttpError::ReadOnly
        }
        (StatusCode::NOT_FOUND, Scope::Session(session_id)) => {
            HttpError::SessionNotFound(session_id.to_string())
        }
        _ => HttpError::Server {
            status,
            body: body.to_string(),
        },
    }
}

/// Only the resolve endpoints mint `ApprovalGone`, and only for a 404 naming
/// this nonce (#1821).
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
        classify_error(status, body, Scope::Session(session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;

    async fn authorization(endpoint: DaemonEndpoint) -> Option<String> {
        let client = HttpClient::new(endpoint).unwrap();
        let request = client
            .authed_builder(client.request(Method::GET, "/api/sessions"))
            .await
            .unwrap()
            .build()
            .unwrap();
        request
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn auth_header() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let base = "http://127.0.0.1:8080".to_string();
        assert_eq!(
            authorization(DaemonEndpoint::new(
                base.clone(),
                Some("tok".into()),
                Source::Env
            ))
            .await,
            Some("Bearer tok".into())
        );
        assert_eq!(
            authorization(DaemonEndpoint::new(base.clone(), None, Source::Env)).await,
            None
        );
        // A local daemon's rotated token file wins over the discovered one.
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("serve.token");
        let rotated = "b".repeat(64);
        std::fs::write(&token_path, &rotated).unwrap();
        let local = DaemonEndpoint::new(base, Some("a".repeat(64)), Source::LocalDaemon)
            .with_local_token_path(token_path);
        assert_eq!(
            authorization(local).await,
            Some(format!("Bearer {rotated}"))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn authed_builder_sends_no_credential_without_token_or_passphrase() {
        // --auth=none: neither a token nor a passphrase resolves, so the
        // request must go out exactly as built, matching pre-passphrase
        // behavior.
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let client = HttpClient::new(DaemonEndpoint::new(
            "http://127.0.0.1:8080".into(),
            None,
            Source::Env,
        ))
        .unwrap();
        let request = client
            .authed_builder(client.http.get("http://127.0.0.1:8080/api/sessions"))
            .await
            .unwrap()
            .build()
            .unwrap();
        assert!(request.headers().get(header::AUTHORIZATION).is_none());
        assert!(request.headers().get(header::COOKIE).is_none());
    }

    #[tokio::test]
    async fn authed_builder_prefers_bearer_token_over_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let passphrase_path = dir.path().join("serve.passphrase");
        std::fs::write(&passphrase_path, "hunter2").unwrap();
        let daemon_endpoint = DaemonEndpoint::new(
            "http://127.0.0.1:8080".into(),
            Some("tok".to_string()),
            Source::LocalDaemon,
        )
        .with_local_passphrase_path(passphrase_path);
        let client = HttpClient::new(daemon_endpoint).unwrap();

        let request = client
            .authed_builder(client.http.get("http://127.0.0.1:8080/api/sessions"))
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer tok"
        );
        assert!(request.headers().get(header::COOKIE).is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn authed_builder_sends_cached_passphrase_session_as_cookie() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let dir = tempfile::tempdir().unwrap();
        let passphrase_path = dir.path().join("serve.passphrase");
        std::fs::write(&passphrase_path, "hunter2").unwrap();
        let daemon_endpoint =
            DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, Source::LocalDaemon)
                .with_local_passphrase_path(passphrase_path);
        let client = HttpClient::new(daemon_endpoint).unwrap();
        client
            .passphrase_session
            .set_for_test(passphrase_session::PassphraseSession {
                cookie: "aoe_session=abc123".to_string(),
                binding_secret: "the-binding-secret".to_string(),
            });

        let request = client
            .authed_builder(client.http.get("http://127.0.0.1:8080/api/sessions"))
            .await
            .unwrap()
            .build()
            .unwrap();
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

    #[test]
    fn error_classification() {
        let session = Scope::Session("s-1");
        let gone =
            |body: &str| classify_resolve_error(StatusCode::NOT_FOUND, body, "abc-123", "s-1");
        assert!(matches!(
            gone("no pending approval with nonce abc-123"),
            HttpError::ApprovalGone
        ));
        assert!(matches!(
            gone("no pending elicitation with nonce abc-123"),
            HttpError::ApprovalGone
        ));
        for body in [
            "no pending approval with nonce other-999",
            "session has no running structured view",
        ] {
            assert!(matches!(gone(body), HttpError::SessionNotFound(s) if s == "s-1"));
        }
        // The shared classifier never mints ApprovalGone.
        assert!(matches!(
            classify_error(StatusCode::NOT_FOUND, "no pending approval with that nonce", session),
            HttpError::SessionNotFound(s) if s == "s-1"
        ));
        assert!(matches!(
            classify_error(StatusCode::NOT_FOUND, "", Scope::Global),
            HttpError::Server { .. }
        ));
        assert!(matches!(
            classify_error(StatusCode::UNAUTHORIZED, "", session),
            HttpError::Unauthorized
        ));
        assert!(matches!(
            classify_error(StatusCode::FORBIDDEN, "daemon is read-only", Scope::Global),
            HttpError::ReadOnly
        ));
        assert!(matches!(
            classify_error(StatusCode::INTERNAL_SERVER_ERROR, "boom", session),
            HttpError::Server { .. }
        ));
        // #1525: the 401 message must not blame a token env var.
        let rendered = HttpError::Unauthorized.to_string();
        assert!(!rendered.contains("AOE_DAEMON_TOKEN") && rendered.contains("401"));
    }
}
