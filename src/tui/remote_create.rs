//! Create sessions on a remote daemon from the new-session dialog.
//!
//! Runs on a worker: the daemon answers only after it has cloned a worktree,
//! started a container or run hooks, which must not stall the TUI.

use std::sync::mpsc::TryRecvError;

use reqwest::StatusCode;

use crate::daemon::{CreateSessionBody, DaemonClient, DaemonClientError};
use crate::session::hook_disclosure::HookDisclosure;
use crate::tui::dialogs::NewSessionData;
use crate::tui::worker::Worker;

/// The daemon route carrying the remote's one-time agent hook acknowledgement.
const HOOKS_ACK_PATH: [&str; 2] = ["app-state", "agent-hooks-acknowledgement"];

pub(crate) struct CreateRequest {
    pub remote: String,
    pub client: DaemonClient,
    pub body: CreateSessionBody,
    /// Record the remote's agent hook acknowledgement first. Set only after
    /// the user approved [`CreateResult::NeedsHookAcknowledgement`].
    pub acknowledge_agent_hooks: bool,
}

/// What one remote create produced.
pub(crate) enum CreateResult {
    Created(String),
    /// The remote owes the one-time acknowledgement of what installing its
    /// agent's hooks writes. Nothing was created; approving and resubmitting
    /// with `acknowledge_agent_hooks` is the whole fix.
    NeedsHookAcknowledgement(Box<HookDisclosure>),
    Failed(String),
}

/// `(remote, result)`.
pub(crate) type CreateOutcome = (String, CreateResult);

/// The remote's answer about its own hook acknowledgement.
#[derive(serde::Deserialize)]
struct HooksAcknowledgement {
    required: bool,
    acknowledged: bool,
    disclosure: Option<HookDisclosure>,
}

pub struct RemoteCreate {
    worker: Worker<CreateRequest, CreateOutcome>,
}

impl RemoteCreate {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        Self {
            worker: Worker::spawn("aoe-remote-create", move |request: CreateRequest| {
                let result = match runtime.as_ref() {
                    Ok(rt) => rt.block_on(run_create(&request)),
                    Err(e) => CreateResult::Failed(format!("no runtime: {e}")),
                };
                (request.remote, result)
            }),
        }
    }

    pub(crate) fn request(&self, request: CreateRequest) {
        self.worker.request(request);
    }

    pub(crate) fn try_recv(&self) -> Result<CreateOutcome, TryRecvError> {
        self.worker.try_recv()
    }
}

/// Ask first, then create: the daemon refuses the launch outright when it owes
/// the acknowledgement, and that refusal still leaves a failed session row
/// behind, so it is worth one extra request to avoid.
async fn run_create(request: &CreateRequest) -> CreateResult {
    if request.acknowledge_agent_hooks {
        if let Err(e) = request
            .client
            .post_api::<_, serde_json::Value>(&HOOKS_ACK_PATH, &serde_json::json!({}))
            .await
        {
            return CreateResult::Failed(format!("could not record the approval: {}", e.summary()));
        }
    } else if let Some(disclosure) = owed_hook_disclosure(&request.client, &request.body).await {
        return CreateResult::NeedsHookAcknowledgement(Box::new(disclosure));
    }
    match request.client.create_session_unpinned(&request.body).await {
        Ok(created) => CreateResult::Created(created.id),
        Err(e) => CreateResult::Failed(create_failure_message(&request.remote, &e)),
    }
}

/// The disclosure the remote still owes for this create, or `None` when it
/// owes none, cannot answer (an older daemon has no such route), or the
/// session is sandboxed and so never writes host hooks.
async fn owed_hook_disclosure(
    client: &DaemonClient,
    body: &CreateSessionBody,
) -> Option<HookDisclosure> {
    if body.sandbox {
        return None;
    }
    let mut query = vec![("tool", body.tool.as_str())];
    if let Some(profile) = body.profile.as_deref() {
        query.push(("profile", profile));
    }
    let answer: HooksAcknowledgement = client.get_api(&HOOKS_ACK_PATH, &query).await.ok()?;
    (answer.required && !answer.acknowledged).then_some(answer.disclosure?)
}

impl Default for RemoteCreate {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a create failed, from the status and the `aoe-error-code` header: the
/// daemon's error body is never read. A bare 403 is most often a repo whose
/// hooks the remote has not trusted, since this dialog has no remote trust
/// prompt to approve them.
fn create_failure_message(remote: &str, error: &DaemonClientError) -> String {
    match error {
        DaemonClientError::Status {
            code: Some(crate::daemon::ApiErrorCode::AgentHooksNotAcknowledged),
            ..
        } => format!(
            "{remote} has never approved what installing its agent's status hooks writes; \
             run `aoe` there once and accept the prompt, or create the session again to \
             approve it from here"
        ),
        DaemonClientError::Status {
            status: StatusCode::FORBIDDEN,
            code: None,
            ..
        } => "the daemon refused the create (HTTP 403); the repo's hooks may need trusting \
              on that machine (`aoe add --trust-hooks` there)"
            .to_string(),
        DaemonClientError::Status {
            status: StatusCode::BAD_REQUEST,
            code: None,
            ..
        } => "the daemon rejected the create (HTTP 400); check the path, branch and profile \
              on that machine"
            .to_string(),
        other => other.summary(),
    }
}

/// The daemon's `POST /api/sessions` body for a dialog submit. Sent without
/// the runtime epoch the local feed pins, since a remote has its own runtime;
/// empty title and profile defer to that daemon's defaults.
pub(crate) fn create_body(data: &NewSessionData) -> CreateSessionBody {
    // `trust_hooks: None` refuses unapproved repo hooks; `false` would skip
    // them instead, and nothing here may approve them.
    let mut body = crate::tui::home::wizard_create_body(data, None);
    body.title = body.title.filter(|title| !title.is_empty());
    body.profile = body.profile.filter(|profile| !profile.is_empty());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_optional_fields_are_omitted_and_view_follows_the_structured_choice() {
        let data = NewSessionData {
            remote: Some("mini".into()),
            profile: String::new(),
            title: String::new(),
            path: "/Users/remote/app".into(),
            group: "work".into(),
            tool: "codex".into(),
            worktree_enabled: true,
            worktree_branch: Some("feat".into()),
            create_new_branch: true,
            base_branch: Some("main".into()),
            extra_repo_paths: Vec::new(),
            sandbox: false,
            sandbox_image: String::new(),
            yolo_mode: false,
            extra_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            scratch: false,
            fork_seed: None,
            structured: false,
        };
        let body = create_body(&data);
        assert_eq!(body.title, None);
        assert_eq!(body.profile, None);
        assert_eq!(body.sandbox_image, None);
        assert_eq!(body.path, "/Users/remote/app");
        assert_eq!(body.tool, "codex");
        assert_eq!(body.worktree_branch.as_deref(), Some("feat"));
        assert_eq!(body.view, crate::session::View::Terminal);
        assert_eq!(body.trust_hooks, None);
    }

    #[test]
    fn a_failed_create_is_described_from_its_status_not_its_body() {
        let status = |status, code| DaemonClientError::Status {
            status,
            code,
            body: "server text".into(),
            truncated: false,
        };
        for (error, expected) in [
            (
                status(StatusCode::FORBIDDEN, None),
                "hooks may need trusting",
            ),
            (status(StatusCode::BAD_REQUEST, None), "check the path"),
            (
                status(
                    StatusCode::FORBIDDEN,
                    Some(crate::daemon::ApiErrorCode::ReadOnly),
                ),
                "read-only",
            ),
            (status(StatusCode::NOT_FOUND, None), "HTTP 404"),
        ] {
            let message = create_failure_message("mini", &error);
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("server text"), "{message}");
        }
    }

    #[test]
    fn the_hook_acknowledgement_code_names_the_remote_and_the_fix() {
        let message = create_failure_message(
            "mini",
            &DaemonClientError::Status {
                status: StatusCode::BAD_REQUEST,
                code: Some(crate::daemon::ApiErrorCode::AgentHooksNotAcknowledged),
                body: "server text".into(),
                truncated: false,
            },
        );
        assert!(message.starts_with("mini has never approved"), "{message}");
        assert!(message.contains("status hooks"), "{message}");
        // The generic 400 advice sent the user chasing the path and branch.
        assert!(!message.contains("check the path"), "{message}");
    }
}
