//! Reusable client and shared wire contract for the daemon REST API.

mod error;
pub use error::{ApiErrorCode, ERROR_CODE_HEADER};

pub(crate) mod lifecycle;
pub mod login;
pub mod remotes;
mod runtime;
mod runtime_connection;
pub use runtime::{
    CreationPhase, CreationProgress, MutationReceipt, ProfileMutation, ProfileSnapshot,
    ProjectMutation, ProjectResponse, ProjectTarget, ReloadFailureCode, RuntimeCapabilities,
    RuntimeContents, RuntimeCursor, RuntimeFrame, RuntimeHealth, RuntimeInfo, RuntimeSnapshot,
    SessionMutation, RUNTIME_EPOCH_HEADER, RUNTIME_PROTOCOL_VERSION, RUNTIME_REVISION_HEADER,
};
pub use runtime_connection::{RuntimeConnection, RuntimeConnectionError, RuntimeEvent};
pub(crate) mod transport;
pub(crate) mod websocket;
pub use websocket::WsError;
mod wire;

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION};
use reqwest::{StatusCode, Url};
use thiserror::Error;

pub use wire::{
    AbandonPurgeBody, AcpWorkerState, CleanupDefaults, CollapseGroupBody,
    ContextResumeAvailability, ContextResumeIndeterminateReason, ContextResumeUnavailableReason,
    CreateProfileBody, CreateProjectBody, CreateSessionBody, CreationTrustFingerprint,
    CreationTrustRequest, CreationTrustReview, DefaultProfileBody, DeleteGroupBody,
    DeleteGroupMode, DeleteGroupOutcome, DeleteProfileQuery, DeleteSessionBody, EnsureToolBody,
    GroupLocation, GroupSessionOutcome, ListSessionsQuery, MoveGroupBody, PendingApproval,
    PlanSummary, PromptAttachmentKind, PromptAttachmentRef, PurgeOutcome, QueuedPromptEntry,
    RenameProfileBody, RepoBaseInput, RestartOutcome, RestartSessionBody, SessionResponse,
    SessionsEnvelope, StartSessionBody, TerminalSize, TerminalTarget, TerminalTargetStatus,
    TrashOutcome, TrashRelocationOutcome, TrashSessionBody, Tristate, UpdateArchiveBody,
    UpdateColorBody, UpdateDiffBaseBody, UpdateFavoriteBody, UpdateGroupBody,
    UpdateNotificationsBody, UpdatePinBody, UpdateSnoozeBody, UpdateUnreadBody,
    WorkspaceRepoSummary,
};

/// Header name carrying the device-binding secret on REST requests. The
/// WebSocket form is an `aoe-device.<secret>` subprotocol instead.
pub const DEVICE_BINDING_HEADER: &str = "x-aoe-device-binding";

/// Session and device-binding pair minted by a passphrase login. Both halves
/// travel together; one without the other is not a credential.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionCredential {
    pub session: String,
    pub binding: String,
}

impl fmt::Debug for SessionCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCredential(<redacted>)")
    }
}

/// Present a login session on a REST request: the cookie plus its binding.
pub(crate) fn insert_login_headers(
    headers: &mut reqwest::header::HeaderMap,
    login: &SessionCredential,
) -> Result<(), DaemonClientError> {
    let sensitive = |value: String| {
        HeaderValue::from_str(&value)
            .map(|mut value| {
                value.set_sensitive(true);
                value
            })
            .map_err(|_| DaemonClientError::InvalidBearerToken)
    };
    headers.insert(
        reqwest::header::COOKIE,
        sensitive(format!("aoe_session={}", login.session))?,
    );
    headers.insert(DEVICE_BINDING_HEADER, sensitive(login.binding.clone())?);
    Ok(())
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// Creating a session can clone a worktree, start a container and run hooks
/// before the daemon answers.
const CREATE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const MAX_SUCCESS_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The trash route nests its outcome one level deep.
#[derive(serde::Deserialize)]
struct TrashResponse {
    outcome: TrashOutcome,
}

/// Client for the daemon's session REST API.
///
/// Clones share the underlying reqwest connection pool.
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    sessions_url: Url,
    authorization: Option<HeaderValue>,
    unix_path: Option<std::path::PathBuf>,
    /// Passphrase-login credential, when the daemon has a login wall.
    login: Option<SessionCredential>,
}

impl fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authenticated = self.is_authenticated();
        let mut debug = f.debug_struct("DaemonClient");
        if authenticated {
            debug.field("sessions_url", &"<redacted>");
        } else {
            debug.field("sessions_url", &self.sessions_url);
        }
        debug.field("authenticated", &authenticated).finish()
    }
}

/// `Retry-After` in whole seconds, when the daemon sent one.
pub(crate) fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// How a lockout reads to the user, with where the failures usually come from.
pub(crate) fn lockout_message(retry_after_secs: Option<u64>) -> String {
    let wait = match retry_after_secs {
        Some(secs) if secs >= 60 => format!(" for {}m", secs.div_ceil(60)),
        Some(secs) => format!(" for {secs}s"),
        None => String::new(),
    };
    format!(
        "locked out{wait} by too many failed attempts; another aoe or browser on this machine \
         may be retrying with old credentials, so run `aoe remote list` and remove stale entries"
    )
}

/// Failure from constructing or calling a [`DaemonClient`].
#[derive(Debug, Error)]
pub enum DaemonClientError {
    /// The supplied URL is not a usable HTTP daemon base URL.
    #[error("invalid daemon base URL: {reason}")]
    InvalidBaseUrl { reason: &'static str },
    #[error("invalid daemon request path segment")]
    InvalidPathSegment,
    /// The bearer token cannot be represented as an HTTP authorization header.
    #[error("invalid daemon bearer token")]
    InvalidBearerToken,
    /// A bearer token or login session was configured for a non-loopback
    /// plaintext URL.
    #[error("daemon credentials require HTTPS or a loopback HTTP URL")]
    InsecureBearerTransport,
    #[error("daemon Unix transport failed")]
    UnixTransport,
    #[error("daemon peer is not owned by the current user")]
    PeerIdentity,
    #[error("daemon request timed out; mutation outcome may be unknown")]
    Timeout,
    /// The default reqwest client could not be built.
    #[error("failed to build daemon HTTP client")]
    ClientBuild,
    #[error("daemon transport error")]
    Transport,
    /// The daemon returned a non-successful HTTP status. Authenticated
    /// responses omit the body so transformed credentials cannot be reflected.
    #[error("daemon returned HTTP {status}: {body}")]
    Status {
        status: StatusCode,
        code: Option<ApiErrorCode>,
        body: String,
        truncated: bool,
    },
    /// The daemon locked this IP out after failed authentication attempts.
    #[error("{}", lockout_message(*retry_after_secs))]
    RateLimited { retry_after_secs: Option<u64> },
    /// A successful response exceeded the bounded sessions-envelope limit.
    #[error("daemon response exceeded the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    /// A successful response did not match the shared wire contract.
    #[error("failed to decode daemon response: {0}")]
    Decode(#[source] serde_json::Error),
    /// An authenticated daemon response did not match the wire contract.
    #[error("failed to decode authenticated daemon response")]
    AuthenticatedDecode,
    #[error("daemon mutation acknowledgment does not match this runtime")]
    InvalidMutationReceipt,
}

impl DaemonClientError {
    /// One line for a person, built from the status and the `AoE-Error-Code`
    /// header, never from a response body.
    pub fn summary(&self) -> String {
        match self {
            Self::Status {
                code: Some(code), ..
            } => match code {
                ApiErrorCode::ReadOnly => "the daemon is read-only",
                ApiErrorCode::AccessPolicyDenied => "the daemon's access policy denied this",
                ApiErrorCode::CityhallMode => "the daemon is in city hall mode",
                ApiErrorCode::LifecycleLocked => "the session is busy with another operation",
                ApiErrorCode::PendingTargetGone => "the target no longer exists",
                ApiErrorCode::RuntimeEpochMismatch => "the daemon restarted; retry",
                ApiErrorCode::ResumeFailed => "the session could not be resumed",
                ApiErrorCode::CreationTrustChanged => "the repo's configuration changed; retry",
                ApiErrorCode::CreationCancelled => "the create was cancelled",
                ApiErrorCode::CreationNotPending => "the session is no longer being created",
                ApiErrorCode::AgentHooksNotAcknowledged => {
                    "the daemon has not acknowledged its agent hook paths"
                }
            }
            .to_string(),
            Self::Status {
                status: StatusCode::UNAUTHORIZED,
                ..
            } => "not authorized (HTTP 401); pair it again with `aoe remote add`".to_string(),
            Self::Status { status, .. } => format!("daemon returned HTTP {status}"),
            other => other.to_string(),
        }
    }
}

impl DaemonClient {
    /// Build a client with a 15-second timeout and redirects disabled.
    ///
    /// Bearer authentication requires HTTPS except for loopback HTTP endpoints.
    pub fn new(base_url: &str, bearer_token: Option<&str>) -> Result<Self, DaemonClientError> {
        Self::with_login(base_url, bearer_token, None, false)
    }

    /// [`Self::new`] plus the session credential from a passphrase login, for
    /// a daemon started with `--remote` (which mandates both factors).
    /// `allow_plaintext` lifts the HTTPS requirement for a remote the user
    /// registered with `--insecure`.
    pub fn with_login(
        base_url: &str,
        bearer_token: Option<&str>,
        login: Option<&SessionCredential>,
        allow_plaintext: bool,
    ) -> Result<Self, DaemonClientError> {
        let sessions_url = sessions_url(base_url)?;
        let authorization = authorization_header(bearer_token)?;
        let authenticated = authorization.is_some() || login.is_some();
        let http = native_http_client(&sessions_url, authenticated, allow_plaintext)?;
        Ok(Self {
            http,
            sessions_url,
            authorization,
            unix_path: None,
            login: login.cloned(),
        })
    }

    /// Authenticated GET of an `/api/*` endpoint beside the sessions list,
    /// named by its path segments, such as `["filesystem", "browse"]`.
    pub async fn get_api<T: serde::de::DeserializeOwned>(
        &self,
        path: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, DaemonClientError> {
        self.request_json(self.http.get(self.api_url(path)?).query(query))
            .await
    }

    /// Authenticated JSON POST to an `/api/*` endpoint beside the sessions list.
    pub async fn post_api<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &[&str],
        body: &B,
    ) -> Result<T, DaemonClientError> {
        self.request_json(self.http.post(self.api_url(path)?).json(body))
            .await
    }

    /// Authenticated DELETE of an `/api/*` endpoint, discarding the body.
    pub async fn delete_api(&self, path: &[&str]) -> Result<(), DaemonClientError> {
        self.request_response(self.http.delete(self.api_url(path)?))
            .await
            .map(drop)
    }

    /// Each segment is escaped, so an id can never change the route.
    fn api_url(&self, path: &[&str]) -> Result<Url, DaemonClientError> {
        let mut relative = String::new();
        for segment in path {
            if !relative.is_empty() {
                relative.push('/');
            }
            relative.push_str(&transport::path_segment(segment)?.to_string());
        }
        // `sessions_url` ends in `api/sessions`, so a relative join lands on a
        // sibling under the same base path.
        self.sessions_url
            .join(&relative)
            .map_err(|_| DaemonClientError::InvalidBaseUrl {
                reason: "invalid API endpoint",
            })
    }

    /// Either credential makes a request authenticated.
    fn is_authenticated(&self) -> bool {
        self.authorization.is_some() || self.login.is_some()
    }

    pub fn new_unix(path: impl Into<std::path::PathBuf>) -> Result<Self, DaemonClientError> {
        let mut client = Self::new("http://localhost", None)?;
        client.unix_path = Some(path.into());
        Ok(client)
    }

    /// Fetch the sessions endpoint, optionally filtered by session state.
    pub async fn list_sessions(
        &self,
        state: Option<crate::session::SessionScope>,
    ) -> Result<SessionsEnvelope, DaemonClientError> {
        let query = ListSessionsQuery { state };
        self.request_json(self.http.get(self.sessions_url.clone()).query(&query))
            .await
    }

    pub async fn runtime_info(&self) -> Result<RuntimeInfo, DaemonClientError> {
        self.get_api(&["runtime"], &[]).await
    }

    pub(crate) async fn local_runtime_ready(
        &self,
        profile: Option<&str>,
    ) -> Result<bool, DaemonClientError> {
        let info = self.runtime_info().await?;
        let namespace = self
            .unix_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::to_str);
        Ok(info.protocol_version == RUNTIME_PROTOCOL_VERSION
            && !info.epoch.is_empty()
            && info.local_owner
            && namespace.is_some_and(|expected| expected == info.namespace)
            && info.health == RuntimeHealth::Healthy
            && !info.profiles.is_empty()
            && profile.is_none_or(|profile| info.profiles.iter().any(|name| name == profile)))
    }

    /// Review executable repository configuration without provisioning or granting trust.
    pub async fn review_creation_trust(
        &self,
        body: &CreationTrustRequest,
        epoch: &str,
    ) -> Result<CreationTrustReview, DaemonClientError> {
        let url = format!("{}/creation-trust", self.sessions_url);
        self.request_json(
            self.http
                .post(url)
                .header(RUNTIME_EPOCH_HEADER, epoch)
                .json(body),
        )
        .await
    }

    /// Create a row; apply its receipt snapshot before selecting or attaching it.
    pub async fn create_session(
        &self,
        body: &CreateSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<SessionResponse>, DaemonClientError> {
        self.request_mutation_with_outcome(
            self.http.post(self.sessions_url.clone()).json(body),
            epoch,
        )
        .await
    }

    /// Create a row on a daemon whose runtime this client does not track, so
    /// no epoch is pinned.
    pub async fn create_session_unpinned(
        &self,
        body: &CreateSessionBody,
    ) -> Result<SessionResponse, DaemonClientError> {
        self.request_json_within(
            self.http.post(self.sessions_url.clone()).json(body),
            CREATE_TIMEOUT,
        )
        .await
    }

    /// Request deferred cancellation; the returned cursor confirms the daemon
    /// accepted the request, not that the rollback has finished.
    pub async fn cancel_creation(
        &self,
        session_id: &str,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let url = format!(
            "{}/{}/creation/cancel",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation(self.http.post(url), epoch).await
    }

    /// Prepare the agent pane; apply the receipt snapshot before using its target.
    pub async fn ensure_agent(
        &self,
        session_id: &str,
        body: &StartSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<TerminalTarget>, DaemonClientError> {
        let url = format!(
            "{}/{}/ensure",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await
    }

    pub async fn ensure_terminal(
        &self,
        session_id: &str,
        index: u32,
        body: &StartSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<TerminalTarget>, DaemonClientError> {
        let url = format!(
            "{}/{}/terminal",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation_with_outcome(
            self.http.post(url).query(&[("index", index)]).json(body),
            epoch,
        )
        .await
    }

    pub async fn ensure_container_terminal(
        &self,
        session_id: &str,
        index: u32,
        body: &StartSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<TerminalTarget>, DaemonClientError> {
        let url = format!(
            "{}/{}/container-terminal",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation_with_outcome(
            self.http.post(url).query(&[("index", index)]).json(body),
            epoch,
        )
        .await
    }

    /// Ensure a daemon-configured foreground tool. Before using the target,
    /// apply its receipt snapshot and recheck the same-host interaction grant.
    pub async fn ensure_tool(
        &self,
        session_id: &str,
        body: &EnsureToolBody,
        epoch: &str,
    ) -> Result<MutationReceipt<TerminalTarget>, DaemonClientError> {
        let url = format!(
            "{}/{}/tools/ensure",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await
    }

    pub async fn restart_session(
        &self,
        session_id: &str,
        body: &RestartSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<RestartOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}/restart",
            self.sessions_url.as_str().trim_end_matches('/'),
            session_id
        );
        self.request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await
    }

    /// Returns only the cursor; row state is adopted from the runtime stream.
    pub async fn mutate_session(
        &self,
        session_id: &str,
        mutation: &SessionMutation,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let url = format!(
            "{}/{}/{}",
            self.sessions_url,
            transport::path_segment(session_id)?,
            mutation.route(),
        );
        let request = match mutation {
            SessionMutation::Start(body) => self.http.post(url).json(body),
            SessionMutation::Restart(body) => self.http.post(url).json(body),
            SessionMutation::AbandonPurge(body) => self.http.post(url).json(body),
            SessionMutation::StopAuxiliary(target) => self.http.post(url).json(target),
            SessionMutation::Stop | SessionMutation::Restore => self.http.post(url),
            _ => self.http.patch(url).json(mutation),
        };
        self.request_mutation(request, epoch).await
    }

    pub async fn trash_session(
        &self,
        session_id: &str,
        body: &TrashSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<TrashOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}/trash",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        let receipt: MutationReceipt<TrashResponse> = self
            .request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
    }

    /// Trash a row on a daemon whose runtime this client does not track, so
    /// no epoch is pinned.
    pub async fn trash_session_unpinned(
        &self,
        session_id: &str,
        body: &TrashSessionBody,
    ) -> Result<TrashOutcome, DaemonClientError> {
        let url = format!(
            "{}/{}/trash",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        let response: TrashResponse = self.request_json(self.http.post(url).json(body)).await?;
        Ok(response.outcome)
    }

    pub async fn purge_session(
        &self,
        session_id: &str,
        body: &DeleteSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<PurgeOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation_with_outcome(self.http.delete(url).json(body), epoch)
            .await
    }

    /// Permanently delete a row on a daemon whose runtime this client does not
    /// track, so no epoch is pinned.
    pub async fn purge_session_unpinned(
        &self,
        session_id: &str,
        body: &DeleteSessionBody,
    ) -> Result<PurgeOutcome, DaemonClientError> {
        let url = format!(
            "{}/{}",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_json(self.http.delete(url).json(body)).await
    }

    pub async fn mutate_project(
        &self,
        mutation: &ProjectMutation,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let request = match mutation {
            ProjectMutation::Create(body) => self
                .http
                .post(
                    self.sessions_url
                        .join("projects")
                        .map_err(|_| DaemonClientError::Transport)?,
                )
                .json(body),
            ProjectMutation::Update {
                target,
                name_or_path,
                patch,
            } => {
                let url = self
                    .sessions_url
                    .join(&format!(
                        "projects/{}",
                        transport::path_segment(name_or_path)?
                    ))
                    .map_err(|_| DaemonClientError::Transport)?;
                self.http.patch(url).query(target).json(patch)
            }
            ProjectMutation::Remove {
                target,
                name_or_path,
            } => {
                let url = self
                    .sessions_url
                    .join(&format!(
                        "projects/{}",
                        transport::path_segment(name_or_path)?
                    ))
                    .map_err(|_| DaemonClientError::Transport)?;
                self.http.delete(url).query(target)
            }
        };
        self.request_mutation(request, epoch).await
    }

    /// Create an explicit group, rejecting an existing path rather than merging it.
    pub async fn create_group(
        &self,
        group: &GroupLocation,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let url = self
            .sessions_url
            .join("groups")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_mutation(self.http.post(url).json(group), epoch)
            .await
    }

    /// Set the persisted collapsed state; repeating a value does not toggle it.
    pub async fn collapse_group(
        &self,
        body: &CollapseGroupBody,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let url = self
            .sessions_url
            .join("groups/collapse")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_mutation(self.http.patch(url).json(body), epoch)
            .await
    }

    /// Rename or move an entire group subtree, including empty groups.
    /// The returned cursor reflects the committed profiles in the runtime snapshot.
    pub async fn move_group(
        &self,
        body: &MoveGroupBody,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let url = self
            .sessions_url
            .join("groups")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_mutation(self.http.patch(url).json(body), epoch)
            .await
    }

    pub async fn delete_group(
        &self,
        body: &DeleteGroupBody,
        epoch: &str,
    ) -> Result<MutationReceipt<DeleteGroupOutcome>, DaemonClientError> {
        let url = self
            .sessions_url
            .join("groups")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_mutation_with_outcome(self.http.delete(url).json(body), epoch)
            .await
    }

    pub async fn mutate_profile(
        &self,
        mutation: &ProfileMutation,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let request = match mutation {
            ProfileMutation::Create(body) => self
                .http
                .post(
                    self.sessions_url
                        .join("profiles")
                        .map_err(|_| DaemonClientError::Transport)?,
                )
                .json(body),
            ProfileMutation::Rename { name, body } => {
                let url = self
                    .sessions_url
                    .join(&format!(
                        "profiles/{}/rename",
                        transport::path_segment(name)?
                    ))
                    .map_err(|_| DaemonClientError::Transport)?;
                self.http.patch(url).json(body)
            }
            ProfileMutation::Delete { name, query } => {
                let url = self
                    .sessions_url
                    .join(&format!("profiles/{}", transport::path_segment(name)?))
                    .map_err(|_| DaemonClientError::Transport)?;
                self.http.delete(url).query(query)
            }
            ProfileMutation::SetDefault(body) => self
                .http
                .patch(
                    self.sessions_url
                        .join("default-profile")
                        .map_err(|_| DaemonClientError::Transport)?,
                )
                .json(body),
        };
        self.request_mutation(request, epoch).await
    }

    async fn request_mutation(
        &self,
        request: reqwest::RequestBuilder,
        epoch: &str,
    ) -> Result<RuntimeCursor, DaemonClientError> {
        let mut response = self
            .request_response(request.header(RUNTIME_EPOCH_HEADER, epoch))
            .await?;
        let cursor = mutation_cursor(response.headers(), epoch)?;
        discard_bounded_body(&mut response).await?;
        Ok(cursor)
    }

    async fn request_mutation_with_outcome<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        epoch: &str,
    ) -> Result<MutationReceipt<T>, DaemonClientError> {
        let response = self
            .request_response(request.header(RUNTIME_EPOCH_HEADER, epoch))
            .await?;
        let cursor = mutation_cursor(response.headers(), epoch)?;
        let outcome = decode_json(response).await?;
        Ok(MutationReceipt { cursor, outcome })
    }

    async fn request_response(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, DaemonClientError> {
        self.send(request, DEFAULT_TIMEOUT).await
    }

    async fn send(
        &self,
        mut request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<reqwest::Response, DaemonClientError> {
        request = request.timeout(timeout);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        let mut request = request.build().map_err(|_| DaemonClientError::Transport)?;
        if let Some(login) = &self.login {
            insert_login_headers(request.headers_mut(), login)?;
        }
        let mut response =
            transport::execute(&self.http, self.unix_path.as_deref(), request).await?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(DaemonClientError::RateLimited {
                retry_after_secs: retry_after_secs(response.headers()),
            });
        }
        if !status.is_success() {
            let code = ApiErrorCode::from_headers(status, response.headers(), false);
            // D1: 409 lifecycle_locked maps from status plus the single finite
            // server-owned header without reading the body. 401 maps from status
            // alone; neither retains a body or token in the error.
            if code == Some(ApiErrorCode::LifecycleLocked) || status == StatusCode::UNAUTHORIZED {
                return Err(DaemonClientError::Status {
                    status,
                    code,
                    body: String::new(),
                    truncated: false,
                });
            }
            if self.is_authenticated() || self.unix_path.is_some() {
                return Err(DaemonClientError::Status {
                    status,
                    code,
                    body: String::new(),
                    truncated: false,
                });
            }
            let (body, truncated) = self.read_error_body(&mut response).await?;
            return Err(DaemonClientError::Status {
                status,
                code,
                body,
                truncated,
            });
        }
        Ok(response)
    }

    async fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, DaemonClientError> {
        self.request_json_within(request, DEFAULT_TIMEOUT).await
    }

    async fn request_json_within<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T, DaemonClientError> {
        let mut response = self.send(request, timeout).await?;
        let body = read_bounded_body(&mut response, MAX_SUCCESS_BODY_BYTES).await?;
        serde_json::from_slice(&body).map_err(|error| self.decode_error(error))
    }

    fn decode_error(&self, error: serde_json::Error) -> DaemonClientError {
        if self.is_authenticated() || self.unix_path.is_some() {
            DaemonClientError::AuthenticatedDecode
        } else {
            DaemonClientError::Decode(error)
        }
    }

    async fn read_error_body(
        &self,
        response: &mut reqwest::Response,
    ) -> Result<(String, bool), DaemonClientError> {
        let mut bytes = Vec::with_capacity(MAX_ERROR_BODY_BYTES);
        let mut truncated = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| DaemonClientError::Transport)?
        {
            let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }

        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        if body.len() > MAX_ERROR_BODY_BYTES {
            truncate_utf8(&mut body, MAX_ERROR_BODY_BYTES);
            truncated = true;
        }
        Ok((body, truncated))
    }
}

fn mutation_cursor(
    headers: &reqwest::header::HeaderMap,
    expected_epoch: &str,
) -> Result<RuntimeCursor, DaemonClientError> {
    fn single<'a>(headers: &'a reqwest::header::HeaderMap, name: &str) -> Option<&'a HeaderValue> {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?;
        values.next().is_none().then_some(value)
    }
    single(headers, RUNTIME_EPOCH_HEADER)
        .filter(|value| !expected_epoch.is_empty() && value.as_bytes() == expected_epoch.as_bytes())
        .ok_or(DaemonClientError::InvalidMutationReceipt)?;
    let revision = single(headers, RUNTIME_REVISION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|revision| *revision > 0)
        .ok_or(DaemonClientError::InvalidMutationReceipt)?;
    Ok(RuntimeCursor {
        epoch: expected_epoch.into(),
        revision,
    })
}

async fn discard_bounded_body(response: &mut reqwest::Response) -> Result<(), DaemonClientError> {
    tokio::time::timeout(DEFAULT_TIMEOUT, async {
        let limit = MAX_SUCCESS_BODY_BYTES;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(DaemonClientError::ResponseTooLarge { limit });
        }
        let mut remaining = limit;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| DaemonClientError::Transport)?
        {
            remaining = remaining
                .checked_sub(chunk.len())
                .ok_or(DaemonClientError::ResponseTooLarge { limit })?;
        }
        Ok(())
    })
    .await
    .map_err(|_| DaemonClientError::Timeout)?
}

pub(crate) async fn decode_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, DaemonClientError> {
    let body = read_bounded_body(&mut response, MAX_SUCCESS_BODY_BYTES).await?;
    serde_json::from_slice(&body).map_err(|_| DaemonClientError::AuthenticatedDecode)
}

pub(crate) async fn decode_text(
    mut response: reqwest::Response,
) -> Result<String, DaemonClientError> {
    let body = read_bounded_body(&mut response, MAX_SUCCESS_BODY_BYTES).await?;
    String::from_utf8(body).map_err(|_| DaemonClientError::AuthenticatedDecode)
}

async fn read_bounded_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, DaemonClientError> {
    tokio::time::timeout(DEFAULT_TIMEOUT, async {
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(DaemonClientError::ResponseTooLarge { limit });
        }
        let capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(limit);
        let mut bytes = Vec::with_capacity(capacity);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| DaemonClientError::Transport)?
        {
            if chunk.len() > limit.saturating_sub(bytes.len()) {
                return Err(DaemonClientError::ResponseTooLarge { limit });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| DaemonClientError::Timeout)?
}

pub(crate) fn native_http_client(
    url: &Url,
    authenticated: bool,
    allow_plaintext: bool,
) -> Result<reqwest::Client, DaemonClientError> {
    ensure_credential_transport(url, authenticated, allow_plaintext)?;
    let mut builder = reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .user_agent(concat!("aoe-daemon-client/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none());
    if is_loopback_url(url) {
        builder = builder.no_proxy();
        if url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("localhost"))
        {
            builder = builder.resolve_to_addrs(
                "localhost",
                &[
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
                ],
            );
        }
    }
    builder.build().map_err(|_| DaemonClientError::ClientBuild)
}

/// Credentials need HTTPS or loopback HTTP unless the caller opted into
/// plaintext for a trusted LAN remote.
pub(crate) fn ensure_credential_transport(
    url: &Url,
    authenticated: bool,
    allow_plaintext: bool,
) -> Result<(), DaemonClientError> {
    if authenticated && !allow_plaintext && url.scheme() == "http" && !is_loopback_url(url) {
        return Err(DaemonClientError::InsecureBearerTransport);
    }
    Ok(())
}

pub(crate) fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let ip_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub(crate) fn native_url(base_url: &str) -> Result<Url, DaemonClientError> {
    let base = Url::parse(base_url).map_err(|_| DaemonClientError::InvalidBaseUrl {
        reason: "could not parse URL",
    })?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "scheme must be http or https",
        });
    }
    if base.host().is_none() {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "URL must include a host",
        });
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "URL must not include credentials",
        });
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(DaemonClientError::InvalidBaseUrl {
            reason: "URL must not include a query or fragment",
        });
    }
    Ok(base)
}

pub(crate) fn sessions_url(base_url: &str) -> Result<Url, DaemonClientError> {
    let mut base = native_url(base_url)?;
    if !base.path().ends_with('/') {
        base.path_segments_mut()
            .map_err(|_| DaemonClientError::InvalidBaseUrl {
                reason: "URL cannot be used as a base",
            })?
            .push("");
    }
    base.join("api/sessions")
        .map_err(|_| DaemonClientError::InvalidBaseUrl {
            reason: "could not join sessions endpoint",
        })
}

pub(crate) fn authorization_header(
    bearer_token: Option<&str>,
) -> Result<Option<HeaderValue>, DaemonClientError> {
    let Some(token) = bearer_token else {
        return Ok(None);
    };
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(DaemonClientError::InvalidBearerToken);
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| DaemonClientError::InvalidBearerToken)?;
    value.set_sensitive(true);
    Ok(Some(value))
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    let mut boundary = max_bytes.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_receipt_requires_an_unambiguous_current_cursor() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(RUNTIME_EPOCH_HEADER, HeaderValue::from_static("current"));
        headers.insert(RUNTIME_REVISION_HEADER, HeaderValue::from_static("7"));
        assert_eq!(
            mutation_cursor(&headers, "current").unwrap(),
            RuntimeCursor {
                epoch: "current".into(),
                revision: 7
            }
        );
        for (name, values) in [
            (RUNTIME_EPOCH_HEADER, &["previous"][..]),
            (RUNTIME_EPOCH_HEADER, &["current", "current"][..]),
            (RUNTIME_REVISION_HEADER, &["7", "7"][..]),
            (RUNTIME_REVISION_HEADER, &["0"][..]),
            (RUNTIME_REVISION_HEADER, &[][..]),
        ] {
            let original = headers.remove(name).unwrap();
            for value in values {
                headers.append(name, HeaderValue::from_static(value));
            }
            assert!(matches!(
                mutation_cursor(&headers, "current"),
                Err(DaemonClientError::InvalidMutationReceipt)
            ));
            headers.remove(name);
            headers.insert(name, original);
        }
    }
    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        path: String,
        epoch: Option<String>,
        body: Vec<u8>,
    }

    async fn serve_mutation_once(
        route: &'static str,
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: &'static str,
        tx: tokio::sync::oneshot::Sender<CapturedRequest>,
    ) -> String {
        let headers = std::sync::Arc::new(headers);
        let body = std::sync::Arc::new(body.to_owned());
        let tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
        let app = axum::Router::new().route(
            route,
            axum::routing::post(move |req: axum::extract::Request| {
                let headers = headers.clone();
                let body = body.clone();
                let tx = tx.clone();
                async move {
                    let (parts, incoming) = req.into_parts();
                    let bytes = axum::body::to_bytes(incoming, 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    if let Some(tx) = tx.lock().expect("capture slot").take() {
                        let _ = tx.send(CapturedRequest {
                            method: parts.method.to_string(),
                            path: parts.uri.path().to_string(),
                            epoch: parts
                                .headers
                                .get(RUNTIME_EPOCH_HEADER)
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_string),
                            body: bytes.to_vec(),
                        });
                    }
                    let mut response =
                        axum::response::Response::new(axum::body::Body::from((*body).clone()));
                    *response.status_mut() =
                        axum::http::StatusCode::from_u16(status).expect("valid test status");
                    for (name, value) in headers.iter() {
                        response
                            .headers_mut()
                            .insert(*name, value.parse().expect("valid test header value"));
                    }
                    response
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind contract-test server");
        let addr = listener.local_addr().expect("contract-test server addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test request");
        });
        format!("http://{addr}")
    }

    async fn next_request(rx: tokio::sync::oneshot::Receiver<CapturedRequest>) -> CapturedRequest {
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("daemon request arrives")
            .expect("request capture")
    }

    #[tokio::test]
    async fn stop_mutation_posts_to_stop_route_with_epoch_header() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/stop",
            200,
            vec![
                (RUNTIME_EPOCH_HEADER, "epoch-1"),
                (RUNTIME_REVISION_HEADER, "3"),
            ],
            "",
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, None).expect("test client");
        let cursor = client
            .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
            .await
            .expect("stop mutation");
        assert_eq!(
            cursor,
            RuntimeCursor {
                epoch: "epoch-1".into(),
                revision: 3,
            }
        );
        let captured = next_request(rx).await;
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.path, "/api/sessions/sess-1/stop");
        assert_eq!(captured.epoch.as_deref(), Some("epoch-1"));
        assert!(captured.body.is_empty(), "stop sends no body");
    }

    #[tokio::test]
    async fn start_mutation_posts_json_body_to_start_route() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/start",
            200,
            vec![
                (RUNTIME_EPOCH_HEADER, "epoch-7"),
                (RUNTIME_REVISION_HEADER, "11"),
            ],
            "",
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, None).expect("test client");
        let cursor = client
            .mutate_session(
                "sess-9",
                &SessionMutation::Start(StartSessionBody::default()),
                "epoch-7",
            )
            .await
            .expect("start mutation");
        assert_eq!(
            cursor,
            RuntimeCursor {
                epoch: "epoch-7".into(),
                revision: 11,
            }
        );
        let captured = next_request(rx).await;
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.path, "/api/sessions/sess-9/start");
        assert_eq!(captured.epoch.as_deref(), Some("epoch-7"));
        let body: serde_json::Value =
            serde_json::from_slice(&captured.body).expect("start sends a JSON body");
        assert_eq!(body, serde_json::json!({}));
    }

    #[tokio::test]
    async fn conflict_lifecycle_locked_maps_without_reading_body() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/stop",
            409,
            vec![(ERROR_CODE_HEADER, "lifecycle_locked")],
            r#"{"error":"lifecycle_busy","message":"Session lifecycle is busy"}"#,
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, None).expect("test client");
        let error = client
            .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
            .await
            .expect_err("conflict must fail");
        assert!(
            matches!(
                error,
                DaemonClientError::Status {
                    status,
                    code: Some(ApiErrorCode::LifecycleLocked),
                    ref body,
                    truncated: false,
                } if status == StatusCode::CONFLICT && body.is_empty()
            ),
            "409 lifecycle_locked must map from status plus header with no body read, got: {error:?}"
        );
        let captured = next_request(rx).await;
        assert_eq!(captured.path, "/api/sessions/sess-1/stop");
    }

    #[tokio::test]
    async fn unauthorized_maps_by_status_without_body_or_token() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/stop",
            401,
            Vec::new(),
            "secret-body",
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, Some("secret-token")).expect("test client");
        let error = client
            .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
            .await
            .expect_err("unauthorized must fail");
        assert!(
            matches!(
                error,
                DaemonClientError::Status {
                    status,
                    code: None,
                    ref body,
                    ..
                } if status == StatusCode::UNAUTHORIZED && body.is_empty()
            ),
            "401 must be identifiable by status alone with no retained body, got: {error:?}"
        );
        assert!(
            !format!("{client:?}").contains("secret-token"),
            "client debug must not leak the bearer token"
        );
        assert!(
            !format!("{error}").contains("secret"),
            "error display must not reflect the response body"
        );
        let captured = next_request(rx).await;
        assert_eq!(captured.path, "/api/sessions/sess-1/stop");
    }

    #[tokio::test]
    async fn unreachable_daemon_surfaces_transport_error() {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind ephemeral port");
            listener.local_addr().expect("ephemeral addr").port()
        };
        let client =
            DaemonClient::new(&format!("http://127.0.0.1:{port}"), None).expect("test client");
        let error = client
            .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
            .await
            .expect_err("unreachable daemon must fail");
        assert!(
            matches!(error, DaemonClientError::Transport),
            "unreachable daemon must surface a transport error with no local write, got: {error:?}"
        );
    }
}
