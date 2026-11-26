//! Wire types and native HTTP client for the Agent of Empires Daemon.

mod endpoint;
mod error;
mod runtime;
mod scope;
pub mod suggestions;
pub mod time;
pub mod transport;
mod values;
mod wire;

pub use endpoint::{DaemonEndpoint, Source};
pub use error::{
    ApiErrorCode, ManagedResumeCause, TerminalSwitchReason, ERROR_CODE_HEADER,
    MANAGED_RESUME_CAUSE_HEADER,
};
pub use runtime::*;
pub use scope::SessionScope;
pub use values::*;
pub use wire::*;

#[cfg(unix)]
use transport::UnixTransport;

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION, COOKIE};
use reqwest::{StatusCode, Url};
use thiserror::Error;

/// Shared budgets for server operations and the Clients waiting for them.
pub mod deadlines {
    use std::time::Duration;

    pub const SANDBOX_PREPARATION_WINDOW: Duration = Duration::from_secs(30 * 60);
    pub const RENAME_ONESHOT_TIMEOUT: Duration = Duration::from_secs(60);
    pub const RENAME_DEADLINE: Duration =
        Duration::from_secs(RENAME_ONESHOT_TIMEOUT.as_secs() + 15);
    pub const PURGE_DEADLINE: Duration = Duration::from_secs(600);
}

use deadlines::{PURGE_DEADLINE, RENAME_DEADLINE, SANDBOX_PREPARATION_WINDOW};
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// A destroy hook may run for minutes per row, so the bulk purge outlives `DEFAULT_TIMEOUT`.
const EMPTY_TRASH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
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
    #[cfg(unix)]
    unix: Option<UnixTransport>,
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

fn committed_failure_message(code: ApiErrorCode) -> &'static str {
    if let Some(message) = code.sandbox_migration_message() {
        return message;
    }
    match code {
        ApiErrorCode::ManagedResumeRefused(cause) => cause.message(),
        ApiErrorCode::PurgeFailed => "Purge failed; apply its committed state before retrying",
        ApiErrorCode::ResumeFailed => {
            "Resume failed after start committed; the conversation is preserved for explicit retry"
        }
        _ => "Start committed but pane preparation failed; apply canonical state before retrying",
    }
}

fn daemon_status_message(status: StatusCode, code: Option<ApiErrorCode>, body: &str) -> String {
    if let Some(message) = code.and_then(ApiErrorCode::sandbox_migration_message) {
        return message.into();
    }
    match code {
        Some(ApiErrorCode::ManagedResumeRefused(cause)) => cause.message().into(),
        Some(ApiErrorCode::AgentHooksNotApproved) =>
            "Agent hook paths have not been approved; run `aoe hooks approve` on the Daemon host before launching".into(),
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
        // The body names the path or branch when this client may read it.
        Some(ApiErrorCode::WorkingDirMissing) if body.is_empty() => {
            "A project or repository path does not exist where the Daemon runs".into()
        }
        Some(ApiErrorCode::BranchNotFound) if body.is_empty() => {
            "The worktree branch was not found".into()
        }
        Some(ApiErrorCode::WorkingDirMissing | ApiErrorCode::BranchNotFound) => body.into(),
        Some(ApiErrorCode::PurgeFailed) => "Session purge failed".into(),
        Some(ApiErrorCode::AccountSwapUnavailable) => "Account swap refused, nothing changed. The target must be another account (a tool with its own `agent_config_dir`) of the same agent, the session needs a recorded Claude conversation and a launch that can carry it, and a terminal session needs `auto_resume_on_restart` on for the target profile".into(),
        Some(ApiErrorCode::LifecycleLocked) => {
            "Session is busy with another operation; nothing changed, retry when it finishes".into()
        }
        Some(ApiErrorCode::TrashIncomplete) => {
            "Trash is in progress or unfinished; if interrupted, retry `aoe rm <id>` before another lifecycle action".into()
        }
        Some(ApiErrorCode::LifecycleSuperseded) => {
            "Session changed after it was selected; it may be partly processed, refresh and check before retrying".into()
        }
        _ => format!("daemon returned HTTP {status}: {body}"),
    }
}

fn public_creation_failure(body: &str, local_owner: bool) -> String {
    #[derive(serde::Deserialize)]
    struct Failure {
        error: String,
        message: String,
        stage: Option<String>,
        reference: Option<String>,
    }
    let Ok(failure) = serde_json::from_str::<Failure>(body) else {
        return "Session creation failed; inspect the Daemon log".to_owned();
    };
    let public = matches!(
        failure.error.as_str(),
        "create_failed"
            | "internal"
            | "hook_failed"
            | "sandbox_unavailable"
            | "sandbox_image_pull_failed"
            | "sandbox_image_update_failed"
            | "working_dir_missing"
            | "branch_not_found"
            | "create_outcome_unknown"
            | "creation_cancelled"
            | "creation_trust_changed"
    );
    if !public && !local_owner {
        return "Session creation refused; inspect the Daemon log".to_owned();
    }
    let mut message = failure.message;
    if let Some(stage) = failure.stage {
        message.push_str(&format!(" [stage: {stage}]"));
    }
    if let Some(reference) = failure.reference {
        message.push_str(&format!(" [reference: {reference}]"));
    }
    message
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
            #[cfg(unix)]
            unix: None,
        })
    }

    fn authenticated(&self) -> bool {
        !self.credentials.is_empty()
    }

    #[cfg(unix)]
    pub fn new_unix(unix: UnixTransport) -> Result<Self, DaemonClientError> {
        let mut client = Self::new("http://localhost", None)?;
        client.unix = Some(unix);
        Ok(client)
    }

    /// Fetch the sessions endpoint, optionally filtered by session state.
    pub async fn list_sessions(
        &self,
        state: Option<SessionScope>,
    ) -> Result<SessionsEnvelope, DaemonClientError> {
        let query = ListSessionsQuery { state };
        self.request_json(self.http.get(self.sessions_url.clone()).query(&query))
            .await
    }

    pub async fn runtime_info(&self) -> Result<RuntimeInfo, DaemonClientError> {
        self.runtime_info_with_peer(None).await
    }

    async fn runtime_info_with_peer(
        &self,
        expected_pid: Option<u32>,
    ) -> Result<RuntimeInfo, DaemonClientError> {
        let url = self
            .sessions_url
            .join("runtime")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_json_with_peer(self.http.get(url), expected_pid)
            .await
    }

    /// Publish and read canonical state from the same Daemon epoch.
    pub async fn runtime_snapshot(
        &self,
        epoch: &str,
    ) -> Result<RuntimeSnapshot, DaemonClientError> {
        let url = self
            .sessions_url
            .join("runtime/snapshot")
            .map_err(|_| DaemonClientError::Transport)?;
        let snapshot: RuntimeSnapshot = self
            .request_json(self.http.get(url).header(RUNTIME_EPOCH_HEADER, epoch))
            .await?;
        if snapshot.cursor.epoch != epoch {
            return Err(DaemonClientError::InvalidMutationReceipt);
        }
        Ok(snapshot)
    }

    /// `GET /api/settings`: the served profile resolved over the machine-wide values.
    pub async fn settings(&self) -> Result<serde_json::Value, DaemonClientError> {
        let url = self
            .sessions_url
            .join("settings")
            .map_err(|_| DaemonClientError::Transport)?;
        self.request_json(self.http.get(url)).await
    }

    /// `PATCH /api/settings`: the daemon routes each leaf to its owning layer.
    pub async fn patch_settings(&self, patch: &serde_json::Value) -> Result<(), DaemonClientError> {
        let url = self
            .sessions_url
            .join("settings")
            .map_err(|_| DaemonClientError::Transport)?;
        let mut response = self
            .request_response(self.http.patch(url).json(patch))
            .await?;
        discard_bounded_body(&mut response).await
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

    #[cfg(unix)]
    pub async fn runtime_info_for_peer(&self, pid: u32) -> Result<RuntimeInfo, DaemonClientError> {
        self.runtime_info_with_peer(Some(pid)).await
    }

    #[cfg(unix)]
    pub fn unix_path(&self) -> Option<&std::path::Path> {
        self.unix.as_ref().map(UnixTransport::path)
    }

    fn is_unix(&self) -> bool {
        #[cfg(unix)]
        {
            self.unix.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Advisory output only; absence never proves a create failed.
    pub async fn creation_progress(
        &self,
        key: &str,
    ) -> Result<wire::CreationOutput, DaemonClientError> {
        let url = format!(
            "{}/create-progress/{}",
            self.sessions_url,
            transport::path_segment(key)?
        );
        self.request_json(self.http.get(url)).await
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
        origin: PromptOrigin,
    ) -> Result<MutationReceipt<SessionResponse>, DaemonClientError> {
        let mut request = self
            .http
            .post(self.sessions_url.clone())
            .timeout(Duration::from_secs(30 * 60))
            .json(body);
        if let Some(origin) = origin.as_header_value() {
            request = request.header(PromptOrigin::HEADER, origin);
        }
        self.request_mutation_with_outcome(request, epoch).await
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
        self.request_mutation_with_outcome(
            self.http
                .post(url)
                .json(body)
                .timeout(SANDBOX_PREPARATION_WINDOW),
            epoch,
        )
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
            self.http
                .post(url)
                .query(&[("index", index)])
                .json(body)
                .timeout(SANDBOX_PREPARATION_WINDOW),
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
        self.request_mutation_with_outcome(
            self.http
                .post(url)
                .json(body)
                .timeout(SANDBOX_PREPARATION_WINDOW),
            epoch,
        )
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
            .request_mutation_with_outcome(
                self.http
                    .post(url)
                    .json(body)
                    .timeout(SANDBOX_PREPARATION_WINDOW),
                epoch,
            )
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
            SessionMutation::Start(body) => self
                .http
                .post(url)
                .json(body)
                .timeout(SANDBOX_PREPARATION_WINDOW),
            SessionMutation::Restart(body) => self
                .http
                .post(url)
                .json(body)
                .timeout(SANDBOX_PREPARATION_WINDOW),
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
                .timeout(RENAME_DEADLINE + DEFAULT_TIMEOUT),
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
            .timeout(PURGE_DEADLINE + DEFAULT_TIMEOUT))
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
                anchored_path,
            } => {
                let url = self
                    .sessions_url
                    .join(&format!(
                        "projects/{}",
                        transport::path_segment(name_or_path)?
                    ))
                    .map_err(|_| DaemonClientError::Transport)?;
                let request = self.http.delete(url).query(target);
                match anchored_path {
                    Some(path) => request.query(&[("path", path)]),
                    None => request,
                }
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

    pub async fn empty_trash(
        &self,
        body: &EmptyTrashBody,
        epoch: &str,
    ) -> Result<MutationReceipt<EmptyTrashOutcome>, DaemonClientError> {
        self.empty_trash_within(body, epoch, EMPTY_TRASH_TIMEOUT)
            .await
    }

    async fn empty_trash_within(
        &self,
        body: &EmptyTrashBody,
        epoch: &str,
        timeout: Duration,
    ) -> Result<MutationReceipt<EmptyTrashOutcome>, DaemonClientError> {
        let url = format!("{}/empty-trash", self.sessions_url);
        self.request_mutation_with_outcome(self.http.post(url).json(body).timeout(timeout), epoch)
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
        !self.authenticated() && (!self.is_unix() || code == Some(ApiErrorCode::CreateHookFailed))
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
        let creating =
            request.method() == reqwest::Method::POST && request.url() == &self.sessions_url;
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
        let response = self.execute(request, expected_peer).await?;
        self.check_response(response, creating, expected_epoch.as_ref())
            .await
    }

    async fn check_response(
        &self,
        mut response: reqwest::Response,
        creating: bool,
        expected_epoch: Option<&HeaderValue>,
    ) -> Result<reqwest::Response, DaemonClientError> {
        let status = response.status();
        if !status.is_success() {
            let code = ApiErrorCode::from_headers(status, response.headers(), false);
            if matches!(
                code,
                Some(
                    ApiErrorCode::StartFailed
                        | ApiErrorCode::SandboxMigrationDeferred
                        | ApiErrorCode::SandboxMigrationFailed
                        | ApiErrorCode::ResumeFailed
                        | ApiErrorCode::ManagedResumeRefused(_)
                        | ApiErrorCode::PurgeFailed
                )
            ) {
                if let Some(epoch) = expected_epoch.and_then(|value| value.to_str().ok()) {
                    if code == Some(ApiErrorCode::PurgeFailed)
                        || code.is_some_and(|code| code.sandbox_migration_message().is_some())
                        || response.headers().contains_key(RUNTIME_EPOCH_HEADER)
                        || response.headers().contains_key(RUNTIME_REVISION_HEADER)
                    {
                        let cursor = mutation_cursor(response.headers(), epoch)?;
                        return Err(DaemonClientError::Committed {
                            cursor,
                            code: code.unwrap(),
                        });
                    }
                } else if code == Some(ApiErrorCode::PurgeFailed)
                    || code.is_some_and(|code| code.sandbox_migration_message().is_some())
                {
                    return Err(DaemonClientError::InvalidMutationReceipt);
                }
            }
            // Finite rejections and 401 never retain a response body.
            if matches!(
                code,
                Some(
                    ApiErrorCode::LifecycleLocked
                        | ApiErrorCode::TrashIncomplete
                        | ApiErrorCode::LifecycleSuperseded
                        | ApiErrorCode::ManagedResumeRefused(_)
                        | ApiErrorCode::SandboxMigrationDeferred
                        | ApiErrorCode::SandboxMigrationFailed
                )
            ) || status == StatusCode::UNAUTHORIZED
            {
                return Err(DaemonClientError::Status {
                    status,
                    code,
                    body: String::new(),
                    truncated: false,
                });
            }
            if creating
                && !(self.is_unix()
                    && code == Some(ApiErrorCode::CreateHookFailed)
                    && self.should_read_error_body(code))
            {
                let (body, truncated) = self.read_error_body(&mut response).await?;
                let body = public_creation_failure(&body, self.is_unix() && !self.authenticated());
                return Err(DaemonClientError::Status {
                    status,
                    code,
                    body,
                    truncated,
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

    async fn execute(
        &self,
        request: reqwest::Request,
        expected_pid: Option<u32>,
    ) -> Result<reqwest::Response, DaemonClientError> {
        #[cfg(unix)]
        if let Some(unix) = &self.unix {
            return unix.execute(request, expected_pid).await;
        }
        if expected_pid.is_some() {
            return Err(DaemonClientError::PeerIdentity);
        }
        transport::execute(&self.http, request).await
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
            if self.authenticated() || self.is_unix() {
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

pub async fn decode_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, DaemonClientError> {
    decode_json_bounded(response, MAX_SUCCESS_BODY_BYTES).await
}

/// Decode JSON with a byte limit, capped at the largest representable byte buffer.
pub async fn decode_json_bounded<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T, DaemonClientError> {
    let body = read_bounded_body(&mut response, limit).await?;
    serde_json::from_slice(&body).map_err(|error| {
        tracing::debug!(target: "daemon.client", %error, "daemon response decode failed");
        DaemonClientError::AuthenticatedDecode
    })
}

pub async fn decode_text(mut response: reqwest::Response) -> Result<String, DaemonClientError> {
    let body = read_bounded_body(&mut response, MAX_SUCCESS_BODY_BYTES).await?;
    String::from_utf8(body).map_err(|error| {
        tracing::debug!(target: "daemon.client", %error, "daemon response decode failed");
        DaemonClientError::AuthenticatedDecode
    })
}

async fn read_bounded_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, DaemonClientError> {
    let limit = limit.min(isize::MAX as usize);
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
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| DaemonClientError::ResponseTooLarge { limit })?;
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

pub fn native_http_client(
    url: &Url,
    authenticated: bool,
) -> Result<reqwest::Client, DaemonClientError> {
    native_http_client_builder(url, authenticated)?
        .build()
        .map_err(|_| DaemonClientError::ClientBuild)
}

pub fn native_http_client_builder(
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

pub fn is_loopback_url(url: &Url) -> bool {
    url.host_str().is_some_and(is_loopback_host)
}

/// The host half of [`is_loopback_url`], for the callers that have read a host
/// rather than a URL. A name equal to `localhost` whatever its case, or a
/// loopback address.
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || is_loopback_address(host)
}

/// The address half, for the callers that need to know whether the peer is the
/// machine they are on rather than whether it is named something friendly. A
/// name is not an answer here, because nothing in this predicate resolves one,
/// so a name the caller cannot check is not a loopback peer: only an address
/// that parses as a loopback `IpAddr`, so the whole of `127.0.0.0/8` and `::1`
/// and nothing else. A bracketed IPv6 authority keeps its brackets in a URL, so
/// they come off before the address is read.
pub fn is_loopback_address(host: &str) -> bool {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

pub fn native_url(base_url: &str) -> Result<Url, DaemonClientError> {
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

pub fn sessions_url(base_url: &str) -> Result<Url, DaemonClientError> {
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

pub fn authorization_header(
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
mod tests;

#[cfg(all(test, unix))]
#[path = "transport_tests.rs"]
mod transport_tests;
