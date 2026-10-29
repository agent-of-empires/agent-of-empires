//! Reusable client and shared wire contract for the daemon REST API.

mod error;
pub use error::{ApiErrorCode, ERROR_CODE_HEADER};

pub(crate) mod lifecycle;
mod runtime;
mod runtime_connection;
pub use runtime::{
    CreationPhase, CreationProgress, MutationReceipt, OwnerLoadDiagnostic, ProfileMutation,
    ProfileSnapshot, ProjectMutation, ProjectResponse, ProjectTarget, ReloadFailureCode,
    RuntimeCapabilities, RuntimeContents, RuntimeCursor, RuntimeFrame, RuntimeHealth, RuntimeInfo,
    RuntimeSnapshot, SessionMutation, RUNTIME_EPOCH_HEADER, RUNTIME_PROTOCOL_VERSION,
    RUNTIME_REVISION_HEADER,
};
pub(crate) use runtime::{NamespaceMutation, NamespaceOutcome};
pub use runtime_connection::{RuntimeConnection, RuntimeConnectionError, RuntimeEvent};
pub(crate) mod transport;
pub(crate) mod websocket;
pub use websocket::WsError;
pub mod live;
mod wire;

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION, COOKIE};
use reqwest::{StatusCode, Url};
use thiserror::Error;

pub use wire::{
    AbandonPurgeBody, AcpWorkerStart, AcpWorkerStartKind, AcpWorkerState, AttachProjectBody,
    AttachProjectOutcome, AttachedProject, AttachedWorkerOutcome, CleanupDefaults,
    CollapseGroupBody, ContextResumeAvailability, ContextResumeIndeterminateReason,
    ContextResumeUnavailableReason, CreateProfileBody, CreateProjectBody, CreateSessionBody,
    CreationTrustFingerprint, CreationTrustRequest, CreationTrustReview, DefaultProfileBody,
    DeleteGroupBody, DeleteGroupMode, DeleteGroupOutcome, DeleteProfileQuery, DeleteSessionBody,
    EnsureToolBody, GroupLocation, GroupSessionFailure, GroupSessionFailureCode,
    GroupSessionOutcome, ListSessionsQuery, MoveDirection, MoveGroupBody, PendingApproval,
    PlanSummary, PromptAttachmentKind, PromptAttachmentRef, PurgeOutcome, QueuedPromptEntry,
    RenameOutcome, RenameProfileBody, RenameSessionBody, ReorderBody, ReorderOutcome,
    RepoBaseInput, RestartOutcome, RestartSessionBody, RestoreOutcome, SessionResponse,
    SessionsEnvelope, SetWorktreeNameBody, StartSessionBody, StopAuxiliaryBody, TerminalSize,
    TerminalTarget, TerminalTargetStatus, TrashOutcome, TrashRelocationOutcome, TrashSessionBody,
    Tristate, UpdateArchiveBody, UpdateColorBody, UpdateDiffBaseBody, UpdateFavoriteBody,
    UpdateGroupBody, UpdateNotificationsBody, UpdatePinBody, UpdateSnoozeBody, UpdateUnreadBody,
    WorkspaceRepoSummary, WorktreeEditOutcome,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const MAX_SUCCESS_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Client for the daemon's session REST API.
///
/// Clones share the underlying reqwest connection pool.
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    sessions_url: Url,
    credentials: Vec<(HeaderName, HeaderValue)>,
    unix_path: Option<std::path::PathBuf>,
}

impl fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authenticated = self.authenticated();
        let mut debug = f.debug_struct("DaemonClient");
        if authenticated {
            debug.field("sessions_url", &"<redacted>");
        } else {
            debug.field("sessions_url", &self.sessions_url);
        }
        debug.field("authenticated", &authenticated).finish()
    }
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
    #[error("invalid daemon session credential")]
    InvalidSessionCredential,
    #[error("daemon credentials require HTTPS or a loopback HTTP URL")]
    InsecureCredentialTransport,
    #[error("daemon Unix transport failed")]
    UnixTransport,
    #[error("daemon peer is not owned by the current user")]
    PeerIdentity,
    #[error("daemon request timed out; mutation outcome may be unknown")]
    Timeout,
    #[error("failed to build daemon HTTP client")]
    ClientBuild,
    #[error("daemon transport error")]
    Transport,
    /// Authenticated responses never reflect credential-bearing bodies.
    #[error("{}", daemon_status_message(*status, *code, body))]
    Status {
        status: StatusCode,
        code: Option<ApiErrorCode>,
        body: String,
        truncated: bool,
    },
    /// Preparation failed after a mutation committed; retain this acknowledgment fence.
    #[error("{}", committed_failure_message(*code))]
    Committed {
        cursor: RuntimeCursor,
        code: ApiErrorCode,
    },
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

#[derive(Debug, Error)]
pub(crate) enum LocalRuntimeError {
    #[error(transparent)]
    Client(#[from] DaemonClientError),
    #[error("Local daemon protocol {actual} does not match {expected}; run `aoe serve --restart`")]
    Protocol { expected: u16, actual: u16 },
    #[error("Local daemon owner or namespace does not match")]
    Identity,
    #[error("Local daemon profile '{0}' is unavailable")]
    ProfileUnavailable(String),
}

fn committed_failure_message(code: ApiErrorCode) -> &'static str {
    match code {
        ApiErrorCode::PurgeFailed => "Purge failed; apply its committed state before retrying",
        ApiErrorCode::ResumeFailed => {
            "Resume failed after start committed; the conversation is preserved for explicit retry"
        }
        _ => "Start committed but pane preparation failed; apply canonical state before retrying",
    }
}

fn daemon_status_message(status: StatusCode, code: Option<ApiErrorCode>, body: &str) -> String {
    match code {
        Some(ApiErrorCode::SmartRenameSkipped) => {
            "Auto-name is unavailable for this session; check its repository and tool configuration"
                .into()
        }
        Some(ApiErrorCode::SmartRenameFailed) => {
            "Auto-name failed; the daemon could not obtain a title from the agent".into()
        }
        Some(ApiErrorCode::SmartRenameTimeout) => {
            "Auto-name timed out waiting for the agent".into()
        }
        Some(ApiErrorCode::ResumeFailed) => {
            "Resume failed; the conversation is preserved for explicit retry".into()
        }
        Some(ApiErrorCode::StartFailed) => "Session start failed".into(),
        Some(ApiErrorCode::PurgeFailed) => "Session purge failed".into(),
        Some(ApiErrorCode::LifecycleLocked) => {
            "Session is busy with another operation; nothing changed, retry when it finishes".into()
        }
        Some(ApiErrorCode::LifecycleSuperseded) => {
            "Session changed after it was selected; it may be partly processed, refresh and check before retrying".into()
        }
        _ => format!("daemon returned HTTP {status}: {body}"),
    }
}

impl DaemonClient {
    /// Build a client with a 15-second timeout and redirects disabled.
    ///
    /// Bearer authentication requires HTTPS except for loopback HTTP endpoints.
    pub fn new(base_url: &str, bearer_token: Option<&str>) -> Result<Self, DaemonClientError> {
        let credentials = authorization_header(bearer_token)?
            .map(|value| vec![(AUTHORIZATION, value)])
            .unwrap_or_default();
        Self::with_credentials(base_url, credentials)
    }

    /// Passphrase-login session: the `aoe_session` cookie pair plus the
    /// device-binding secret. Same transport rules as a bearer token.
    pub fn with_session(
        base_url: &str,
        cookie: &str,
        binding_secret: &str,
    ) -> Result<Self, DaemonClientError> {
        Self::with_credentials(
            base_url,
            vec![
                (COOKIE, sensitive_header(cookie)?),
                (
                    HeaderName::from_static("x-aoe-device-binding"),
                    sensitive_header(binding_secret)?,
                ),
            ],
        )
    }

    fn with_credentials(
        base_url: &str,
        credentials: Vec<(HeaderName, HeaderValue)>,
    ) -> Result<Self, DaemonClientError> {
        let sessions_url = sessions_url(base_url)?;
        let http = native_http_client(&sessions_url, !credentials.is_empty())?;
        Ok(Self {
            http,
            sessions_url,
            credentials,
            unix_path: None,
        })
    }

    fn authenticated(&self) -> bool {
        !self.credentials.is_empty()
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
        let url = self
            .sessions_url
            .join("runtime")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_json(self.http.get(url)).await
    }

    /// The daemon's own default profile, which can differ from this client's config.
    pub async fn default_profile(&self) -> Result<String, DaemonClientError> {
        let url = self
            .sessions_url
            .join("profiles")
            .map_err(|_| DaemonClientError::Transport)?;
        let rows: Vec<ProfileRow> = self.request_json(self.http.get(url)).await?;
        default_of(rows).ok_or(DaemonClientError::Transport)
    }

    pub(crate) async fn local_runtime_ready(
        &self,
        profile: Option<&str>,
    ) -> Result<RuntimeInfo, LocalRuntimeError> {
        let info = self.runtime_info().await?;
        if info.protocol_version != RUNTIME_PROTOCOL_VERSION {
            return Err(LocalRuntimeError::Protocol {
                expected: RUNTIME_PROTOCOL_VERSION,
                actual: info.protocol_version,
            });
        }
        if !self.matches_local_identity(&info) {
            return Err(LocalRuntimeError::Identity);
        }
        if info.health == RuntimeHealth::Healthy
            && (info.profiles.is_empty()
                || profile.is_some_and(|profile| !info.profiles.iter().any(|name| name == profile)))
        {
            return Err(LocalRuntimeError::ProfileUnavailable(
                profile.unwrap_or("default").into(),
            ));
        }
        Ok(info)
    }

    pub(crate) async fn local_daemon_ready(&self, pid: u32) -> Result<bool, DaemonClientError> {
        let url = self
            .sessions_url
            .join("runtime")
            .map_err(|_| DaemonClientError::Transport)?;
        let info: RuntimeInfo = self
            .request_json_with_peer(self.http.get(url), Some(pid))
            .await?;
        Ok(info.protocol_version == RUNTIME_PROTOCOL_VERSION && self.matches_local_identity(&info))
    }

    fn matches_local_identity(&self, info: &RuntimeInfo) -> bool {
        let namespace = self
            .unix_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::to_str);
        !info.epoch.is_empty()
            && info.local_owner
            && namespace.is_some_and(|expected| expected == info.namespace)
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
            transport::path_segment(session_id)?
        );
        #[derive(serde::Deserialize)]
        struct Body {
            outcome: RestartOutcome,
        }
        let receipt: MutationReceipt<Body> = self
            .request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
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
            SessionMutation::Stop => self.http.post(url),
            SessionMutation::Access => self.http.patch(url),
            _ => self.http.patch(url).json(mutation),
        };
        self.request_mutation(request, epoch).await
    }

    pub async fn restore_session(
        &self,
        session_id: &str,
        epoch: &str,
    ) -> Result<MutationReceipt<RestoreOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}/restore",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        #[derive(serde::Deserialize)]
        struct Body {
            outcome: RestoreOutcome,
        }
        let receipt: MutationReceipt<Body> = self
            .request_mutation_with_outcome(self.http.post(url), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
    }

    pub async fn rename_session(
        &self,
        session_id: &str,
        body: &RenameSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<RenameOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        #[derive(serde::Deserialize)]
        struct Body {
            outcome: RenameOutcome,
        }
        let receipt: MutationReceipt<Body> = self
            .request_mutation_with_outcome(self.http.patch(url).json(body), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
    }

    pub async fn set_worktree_name(
        &self,
        session_id: &str,
        body: &SetWorktreeNameBody,
        epoch: &str,
    ) -> Result<MutationReceipt<WorktreeEditOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}/worktree-name",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        #[derive(serde::Deserialize)]
        struct Body {
            outcome: WorktreeEditOutcome,
        }
        let receipt: MutationReceipt<Body> = self
            .request_mutation_with_outcome(self.http.patch(url).json(body), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
    }

    pub async fn attach_session_project(
        &self,
        session_id: &str,
        body: &AttachProjectBody,
        epoch: &str,
    ) -> Result<MutationReceipt<AttachProjectOutcome>, DaemonClientError> {
        let url = format!(
            "{}/{}/projects",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        #[derive(serde::Deserialize)]
        struct Body {
            outcome: AttachProjectOutcome,
        }
        let receipt: MutationReceipt<Body> = self
            .request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
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
        #[derive(serde::Deserialize)]
        struct Body {
            outcome: TrashOutcome,
        }
        let receipt: MutationReceipt<Body> = self
            .request_mutation_with_outcome(self.http.post(url).json(body), epoch)
            .await?;
        Ok(MutationReceipt {
            cursor: receipt.cursor,
            outcome: receipt.outcome.outcome,
        })
    }

    /// Request daemon-owned naming; apply the receipt snapshot before using changed titles.
    pub async fn smart_rename_session(
        &self,
        session_id: &str,
        epoch: &str,
    ) -> Result<MutationReceipt<SessionResponse>, DaemonClientError> {
        let url = format!(
            "{}/{}/smart-rename",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        self.request_mutation_with_outcome(
            self.http
                .post(url)
                .timeout(crate::session::smart_rename::RENAME_DEADLINE + DEFAULT_TIMEOUT),
            epoch,
        )
        .await
    }

    pub async fn purge_session(
        &self,
        session_id: &str,
        body: &DeleteSessionBody,
        epoch: &str,
    ) -> Result<MutationReceipt<PurgeOutcome>, DaemonClientError> {
        self.request_mutation_with_outcome(self.purge_request(session_id, body)?, epoch)
            .await
    }

    /// Purge runs user hooks, so it outlives the default RPC deadline.
    fn purge_request(
        &self,
        session_id: &str,
        body: &DeleteSessionBody,
    ) -> Result<reqwest::RequestBuilder, DaemonClientError> {
        let url = format!(
            "{}/{}",
            self.sessions_url,
            transport::path_segment(session_id)?
        );
        Ok(self
            .http
            .delete(url)
            .json(body)
            .timeout(crate::session::deletion::PURGE_DEADLINE + DEFAULT_TIMEOUT))
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

    pub async fn reorder(
        &self,
        body: &ReorderBody,
        epoch: &str,
    ) -> Result<MutationReceipt<ReorderOutcome>, DaemonClientError> {
        let url = self
            .sessions_url
            .join("reorder")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_mutation_with_outcome(self.http.post(url).json(body), epoch)
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

    fn should_read_error_body(&self, code: Option<ApiErrorCode>) -> bool {
        !self.authenticated()
            && (self.unix_path.is_none() || code == Some(ApiErrorCode::CreateHookFailed))
    }

    async fn request_response(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, DaemonClientError> {
        self.request_response_with_peer(request, None).await
    }

    async fn request_response_with_peer(
        &self,
        mut request: reqwest::RequestBuilder,
        expected_peer: Option<u32>,
    ) -> Result<reqwest::Response, DaemonClientError> {
        for (name, value) in &self.credentials {
            request = request.header(name.clone(), value.clone());
        }
        let mut request = request.build().map_err(|_| DaemonClientError::Transport)?;
        if request.timeout().is_none() {
            *request.timeout_mut() = Some(DEFAULT_TIMEOUT);
        }
        let expected_epoch = {
            let mut values = request.headers().get_all(RUNTIME_EPOCH_HEADER).iter();
            let value = values.next();
            if values.next().is_none() {
                value.cloned()
            } else {
                None
            }
        };
        let mut response = transport::execute_with_peer(
            &self.http,
            self.unix_path.as_deref(),
            request,
            expected_peer,
        )
        .await?;
        let status = response.status();
        if !status.is_success() {
            let code = ApiErrorCode::from_headers(status, response.headers(), false);
            if matches!(
                code,
                Some(
                    ApiErrorCode::StartFailed
                        | ApiErrorCode::ResumeFailed
                        | ApiErrorCode::PurgeFailed
                )
            ) {
                if let Some(epoch) = expected_epoch
                    .as_ref()
                    .and_then(|value| value.to_str().ok())
                {
                    if code == Some(ApiErrorCode::PurgeFailed)
                        || response.headers().contains_key(RUNTIME_EPOCH_HEADER)
                        || response.headers().contains_key(RUNTIME_REVISION_HEADER)
                    {
                        let cursor = mutation_cursor(response.headers(), epoch)?;
                        return Err(DaemonClientError::Committed {
                            cursor,
                            code: code.unwrap(),
                        });
                    }
                } else if code == Some(ApiErrorCode::PurgeFailed) {
                    return Err(DaemonClientError::InvalidMutationReceipt);
                }
            }
            // D1: 409 lifecycle_locked maps from status plus the single finite
            // server-owned header without reading the body. 401 maps from status
            // alone; neither retains a body or token in the error.
            if matches!(
                code,
                Some(ApiErrorCode::LifecycleLocked | ApiErrorCode::LifecycleSuperseded)
            ) || status == StatusCode::UNAUTHORIZED
            {
                return Err(DaemonClientError::Status {
                    status,
                    code,
                    body: String::new(),
                    truncated: false,
                });
            }
            if !self.should_read_error_body(code) {
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
        self.request_json_with_peer(request, None).await
    }

    async fn request_json_with_peer<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        expected_peer: Option<u32>,
    ) -> Result<T, DaemonClientError> {
        let mut response = self
            .request_response_with_peer(request, expected_peer)
            .await?;
        let body = read_bounded_body(&mut response, MAX_SUCCESS_BODY_BYTES).await?;
        serde_json::from_slice(&body).map_err(|error| {
            if self.authenticated() || self.unix_path.is_some() {
                DaemonClientError::AuthenticatedDecode
            } else {
                DaemonClientError::Decode(error)
            }
        })
    }

    async fn read_error_body(
        &self,
        response: &mut reqwest::Response,
    ) -> Result<(String, bool), DaemonClientError> {
        tokio::time::timeout_at(transport::response_deadline(response), async {
            let mut bytes = Vec::with_capacity(MAX_ERROR_BODY_BYTES);
            let mut truncated = false;
            while let Some(chunk) = response.chunk().await.map_err(transport::request_error)? {
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
        })
        .await
        .map_err(|_| DaemonClientError::Timeout)?
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
    tokio::time::timeout_at(transport::response_deadline(response), async {
        let limit = MAX_SUCCESS_BODY_BYTES;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(DaemonClientError::ResponseTooLarge { limit });
        }
        let mut remaining = limit;
        while let Some(chunk) = response.chunk().await.map_err(transport::request_error)? {
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
    response: reqwest::Response,
) -> Result<T, DaemonClientError> {
    decode_json_bounded(response, MAX_SUCCESS_BODY_BYTES).await
}

pub(crate) async fn decode_json_bounded<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T, DaemonClientError> {
    let body = read_bounded_body(&mut response, limit).await?;
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
    tokio::time::timeout_at(transport::response_deadline(response), async {
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
        while let Some(chunk) = response.chunk().await.map_err(transport::request_error)? {
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
) -> Result<reqwest::Client, DaemonClientError> {
    native_http_client_builder(url, authenticated)?
        .build()
        .map_err(|_| DaemonClientError::ClientBuild)
}

pub(crate) fn native_http_client_builder(
    url: &Url,
    authenticated: bool,
) -> Result<reqwest::ClientBuilder, DaemonClientError> {
    if authenticated && url.scheme() == "http" && !is_loopback_url(url) {
        return Err(DaemonClientError::InsecureCredentialTransport);
    }
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
    Ok(builder)
}

pub(crate) fn is_loopback_url(url: &Url) -> bool {
    url.host_str().is_some_and(is_loopback_host)
}

/// The host half of [`is_loopback_url`], for the callers that have read a host
/// rather than a URL. A name equal to `localhost` whatever its case, or a
/// loopback address.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || is_loopback_address(host)
}

/// The address half, for the callers that need to know whether the peer is the
/// machine they are on rather than whether it is named something friendly. A
/// name is not an answer here, because nothing in this predicate resolves one,
/// so a name the caller cannot check is not a loopback peer: only an address
/// that parses as a loopback `IpAddr`, so the whole of `127.0.0.0/8` and `::1`
/// and nothing else. A bracketed IPv6 authority keeps its brackets in a URL, so
/// they come off before the address is read.
pub(crate) fn is_loopback_address(host: &str) -> bool {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
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

fn sensitive_header(value: &str) -> Result<HeaderValue, DaemonClientError> {
    let mut value =
        HeaderValue::from_str(value).map_err(|_| DaemonClientError::InvalidSessionCredential)?;
    value.set_sensitive(true);
    Ok(value)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    let mut boundary = max_bytes.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[derive(serde::Deserialize)]
struct ProfileRow {
    name: String,
    is_default: bool,
}

fn default_of(rows: Vec<ProfileRow>) -> Option<String> {
    rows.into_iter().find(|r| r.is_default).map(|r| r.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_the_row_the_daemon_marks() {
        let rows: Vec<ProfileRow> = serde_json::from_str(
            r#"[{"name":"main","is_default":false},{"name":"work","is_default":true,"description":"x"}]"#,
        )
        .unwrap();
        assert_eq!(default_of(rows).as_deref(), Some("work"));
        assert_eq!(default_of(Vec::new()), None);
    }

    #[tokio::test]
    async fn native_admission_preserves_degraded_reads_without_relaxing_identity() {
        let root = tempfile::tempdir().unwrap();
        let daemon = root.path().join("daemon");
        std::fs::create_dir(&daemon).unwrap();
        let path = daemon.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let info = std::sync::Arc::new(tokio::sync::RwLock::new(RuntimeInfo {
            owner_diagnostics: Vec::new(),
            owner_instance_id: None,
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            epoch: "native-epoch".into(),
            namespace: root.path().to_str().unwrap().into(),
            tmux_socket: None,
            local_owner: true,
            profiles: vec!["healthy".into()],
            read_only: false,
            cityhall_mode: false,
            health: RuntimeHealth::Degraded {
                code: ReloadFailureCode::ProfileData,
                profiles: vec!["damaged".into()],
            },
            interaction_capabilities: RuntimeCapabilities {
                mutations: false,
                native_interaction: false,
            },
        }));
        let served_info = info.clone();
        let router = axum::Router::new().route(
            "/api/runtime",
            axum::routing::get(move || {
                let info = served_info.clone();
                async move { axum::Json(info.read().await.clone()) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = DaemonClient::new_unix(path).unwrap();
        assert!(client.local_daemon_ready(std::process::id()).await.unwrap());
        for profile in ["healthy", "damaged"] {
            let admitted = client.local_runtime_ready(Some(profile)).await.unwrap();
            assert!(matches!(admitted.health, RuntimeHealth::Degraded { .. }));
            assert!(!admitted.interaction_capabilities.mutations);
            assert!(!admitted.interaction_capabilities.native_interaction);
        }
        assert!(matches!(
            client.local_daemon_ready(std::process::id() + 1).await,
            Err(DaemonClientError::PeerIdentity)
        ));
        info.write().await.protocol_version = RUNTIME_PROTOCOL_VERSION + 1;
        assert!(matches!(
            client.local_runtime_ready(None).await,
            Err(LocalRuntimeError::Protocol { expected, actual })
                if expected == RUNTIME_PROTOCOL_VERSION && actual == RUNTIME_PROTOCOL_VERSION + 1
        ));
        info.write().await.protocol_version = RUNTIME_PROTOCOL_VERSION;
        info.write().await.namespace = "different-namespace".into();
        assert!(matches!(
            client.local_runtime_ready(None).await,
            Err(LocalRuntimeError::Identity)
        ));
        info.write().await.namespace = root.path().to_str().unwrap().into();
        info.write().await.health = RuntimeHealth::Healthy;
        assert!(matches!(
            client.local_runtime_ready(Some("missing")).await,
            Err(LocalRuntimeError::ProfileUnavailable(profile)) if profile == "missing"
        ));
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn error_body_policy_exposes_hook_failures_only_to_local_owner() {
        let unix = DaemonClient::new_unix("/tmp/aoe-test.sock").expect("unix client");
        let bearer = DaemonClient::new("http://localhost", Some("token")).expect("bearer client");
        let anonymous = DaemonClient::new("http://localhost", None).expect("anonymous client");

        assert!(unix.should_read_error_body(Some(ApiErrorCode::CreateHookFailed)));
        assert!(!unix.should_read_error_body(None));
        assert!(!bearer.should_read_error_body(Some(ApiErrorCode::CreateHookFailed)));
        assert!(anonymous.should_read_error_body(None));
    }
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
            axum::routing::any(move |req: axum::extract::Request| {
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
    async fn restart_request_encodes_the_session_id_as_one_path_segment() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/restart",
            200,
            vec![
                (RUNTIME_EPOCH_HEADER, "epoch-1"),
                (RUNTIME_REVISION_HEADER, "2"),
            ],
            r#"{"id":"sess/a?b","lifecycle_generation":4,"profile":"root-profile","outcome":{"lifecycle_generation":5,"profile":"target-profile","target":{"tmux_session":"prepared-pane","status":"restarted","lifecycle_generation":5,"profile":"target-profile"}}}"#,
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, None).unwrap();
        let receipt = client
            .restart_session("sess/a?b", &RestartSessionBody::default(), "epoch-1")
            .await
            .unwrap();
        assert_eq!(receipt.cursor.epoch, "epoch-1");
        assert_eq!(receipt.cursor.revision, 2);
        assert_eq!(receipt.outcome.lifecycle_generation, 5);
        assert_eq!(receipt.outcome.profile, "target-profile");
        let target = receipt.outcome.target.unwrap();
        assert_eq!(target.tmux_session, "prepared-pane");
        assert_eq!(target.lifecycle_generation, 5);
        assert_eq!(target.profile, "target-profile");
        let captured = next_request(rx).await;
        assert_eq!(captured.path, "/api/sessions/sess%2Fa%3Fb/restart");
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
        assert!(
            error.to_string().contains("busy with another operation"),
            "TUI-facing text must say the session is busy, got: {error}"
        );
        let captured = next_request(rx).await;
        assert_eq!(captured.path, "/api/sessions/sess-1/stop");
    }

    #[tokio::test]
    async fn conflict_lifecycle_superseded_stays_distinct_from_busy() {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/stop",
            409,
            vec![(ERROR_CODE_HEADER, "lifecycle_superseded")],
            r#"{"error":"lifecycle_superseded","message":"changed"}"#,
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, None).expect("test client");
        let error = client
            .mutate_session("sess-1", &SessionMutation::Stop, "epoch-1")
            .await
            .expect_err("conflict must fail");
        assert!(matches!(
            error,
            DaemonClientError::Status {
                code: Some(ApiErrorCode::LifecycleSuperseded),
                ref body,
                ..
            } if body.is_empty()
        ));
        let text = error.to_string();
        assert!(text.contains("changed after it was selected"), "{text}");
        assert!(!text.contains("nothing changed"), "{text}");
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
    async fn smart_rename_accepts_completed_and_supervised_receipts() {
        for status in [200, 202] {
            let row = SessionResponse::from_instance(
                &crate::session::Instance::new("named", "/tmp/named"),
                false,
            );
            let app = axum::Router::new().route(
                "/api/sessions/{id}/smart-rename",
                axum::routing::post(move |req: axum::extract::Request| {
                    let row = row.clone();
                    async move {
                        assert_eq!(req.headers()[RUNTIME_EPOCH_HEADER], "epoch-1");
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            [
                                (RUNTIME_EPOCH_HEADER, "epoch-1"),
                                (RUNTIME_REVISION_HEADER, "3"),
                            ],
                            axum::Json(row),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client = DaemonClient::new(&format!("http://{address}"), None).unwrap();
            let receipt = client
                .smart_rename_session("sess-1", "epoch-1")
                .await
                .unwrap();
            assert_eq!(receipt.cursor.epoch, "epoch-1");
            assert_eq!(receipt.cursor.revision, 3);
            assert_eq!(receipt.outcome.title, "named");
            server.abort();
            let _ = server.await;
        }
    }

    #[test]
    fn purge_waits_past_the_default_timeout_for_hooks() {
        let client = DaemonClient::new("http://127.0.0.1:1", None).unwrap();
        let request = client
            .purge_request("sess-1", &DeleteSessionBody::default())
            .unwrap()
            .build()
            .unwrap();
        let timeout = request.timeout().copied().unwrap();
        assert_eq!(
            timeout,
            crate::session::deletion::PURGE_DEADLINE + DEFAULT_TIMEOUT
        );
        assert!(timeout > DEFAULT_TIMEOUT);
    }

    #[tokio::test]
    async fn committed_mutation_errors_require_finite_code_and_matching_cursor() {
        for (status, code, epoch, revision, committed, invalid) in [
            (500, "start_failed", "epoch-1", "3", true, false),
            (409, "resume_failed", "epoch-1", "3", true, false),
            (409, "purge_failed", "epoch-1", "3", true, false),
            (409, "purge_failed", "foreign", "3", false, true),
            (409, "purge_failed", "epoch-1", "0", false, true),
            (409, "purge_failed", "absent", "absent", false, true),
            (500, "purge_failed", "epoch-1", "3", false, false),
            (409, "resume_failed", "foreign", "3", false, true),
            (409, "resume_failed", "epoch-1", "0", false, true),
            (409, "resume_failed", "epoch-1", "3, 4", false, true),
            (409, "unknown", "epoch-1", "3", false, false),
            (403, "resume_failed", "epoch-1", "3", false, false),
        ] {
            let purge = code == "purge_failed";
            let (tx, rx) = tokio::sync::oneshot::channel();
            let base = serve_mutation_once(
                if purge {
                    "/api/sessions/{id}"
                } else {
                    "/api/sessions/{id}/start"
                },
                status,
                if epoch == "absent" {
                    vec![(ERROR_CODE_HEADER, code)]
                } else {
                    vec![
                        (ERROR_CODE_HEADER, code),
                        (RUNTIME_EPOCH_HEADER, epoch),
                        (RUNTIME_REVISION_HEADER, revision),
                    ]
                },
                "secret-body",
                tx,
            )
            .await;
            let client = DaemonClient::new(&base, Some("secret-token")).unwrap();
            let error = if purge {
                client
                    .purge_session("sess-1", &DeleteSessionBody::default(), "epoch-1")
                    .await
                    .unwrap_err()
            } else {
                client
                    .mutate_session(
                        "sess-1",
                        &SessionMutation::Start(StartSessionBody::default()),
                        "epoch-1",
                    )
                    .await
                    .unwrap_err()
            };
            assert!(!error.to_string().contains("secret"));
            if committed {
                assert!(
                    matches!(error, DaemonClientError::Committed { cursor, .. } if cursor.epoch == "epoch-1" && cursor.revision == 3)
                );
            } else if invalid {
                assert!(matches!(error, DaemonClientError::InvalidMutationReceipt));
            } else {
                assert!(matches!(error, DaemonClientError::Status { .. }));
            }
            assert_eq!(next_request(rx).await.epoch.as_deref(), Some("epoch-1"));
        }
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let base = serve_mutation_once(
            "/api/sessions/{id}/start",
            409,
            vec![
                (ERROR_CODE_HEADER, "resume_failed"),
                (RUNTIME_EPOCH_HEADER, "epoch-1"),
                (RUNTIME_REVISION_HEADER, "3"),
            ],
            "secret-body",
            tx,
        )
        .await;
        let client = DaemonClient::new(&base, Some("secret-token")).unwrap();
        let error = client
            .request_response(
                client
                    .http
                    .post(format!("{base}/api/sessions/sess-1/start")),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, DaemonClientError::Status { .. }),
            "a non-mutation request cannot adopt an error ACK"
        );
    }

    #[test]
    fn smart_rename_errors_do_not_reflect_agent_output() {
        for code in [
            ApiErrorCode::SmartRenameSkipped,
            ApiErrorCode::SmartRenameFailed,
            ApiErrorCode::SmartRenameTimeout,
        ] {
            let error = DaemonClientError::Status {
                status: code.status(),
                code: Some(code),
                body: "secret-agent-output".into(),
                truncated: false,
            };
            assert!(!error.to_string().contains("secret"));
        }
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
