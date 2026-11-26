//! Client side of the daemon's suggestions API (`/api/suggestions`): the
//! snapshot and prepare wire types and the [`DaemonClient`] calls that read
//! and act on them.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;

use super::{DaemonClient, DaemonClientError, DEFAULT_TIMEOUT};

/// `POST /api/suggestions/start` answers after the create, which can take
/// minutes for a sandbox; `POST /api/sessions` waits as long.
const START_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A refusal's explanation is bounded before it reaches the screen.
const MAX_REFUSAL_BYTES: usize = 4096;
const MAX_REFUSAL_CHARS: usize = 200;

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    /// The profile the daemon publishes suggestions under.
    #[serde(default)]
    pub profile: Option<String>,
    /// The daemon that served this snapshot; set by the loader, not the wire.
    #[serde(skip)]
    pub daemon: Option<String>,
    /// The daemon's `pre_trust_agent_folders`; set by the loader, not the wire.
    #[serde(skip)]
    pub pre_trust: Option<bool>,
    pub providers: Vec<Provider>,
    pub items: Vec<Item>,
    #[serde(default)]
    pub prefs: Prefs,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub show_source_ref: bool,
    /// "Show in the sidebar": the web sidebar group and the TUI Home row.
    pub sidebar: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            show_source_ref: true,
            sidebar: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Unix seconds of the last report.
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub refreshing: bool,
    #[serde(default)]
    pub checks: Vec<Check>,
}

/// A provider's setup check; `fix` is the plugin's own untrusted advice.
#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    pub status: String,
    #[serde(default)]
    pub fix: String,
}

/// What Start creates an item with, resolved against its Project.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemStart {
    pub agent: String,
    pub view: String,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub yolo: bool,
    /// The first prompt Start sends; absent when the expanded template is too long.
    #[serde(default)]
    pub prompt: Option<String>,
}

fn enabled_by_default() -> bool {
    true
}

/// The registered Project an item resolves to.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedProject {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub provider_id: String,
    pub id: String,
    pub title: String,
    pub reason: String,
    pub source_url: String,
    pub source_ref: String,
    pub preview: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub dismissed: bool,
    pub draft: Draft,
    #[serde(default)]
    pub started_session: Option<Started>,
    #[serde(default)]
    pub resolved_project: Option<ResolvedProject>,
    /// The repo the provider links to a Project, as `owner/repo` for GitHub.
    #[serde(default)]
    pub mapping_key: Option<String>,
    #[serde(default)]
    pub start: Option<ItemStart>,
    /// The source closed or changed; the item cannot start.
    #[serde(default)]
    pub stale: bool,
}

/// The live Session an item was started as.
#[derive(Debug, Clone, Deserialize)]
pub struct Started {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub archived: bool,
    /// `"start"` for a one-click Start; only those may be trashed and started again.
    #[serde(default)]
    pub started_via: Option<String>,
}

impl Started {
    pub fn one_click(&self) -> bool {
        self.started_via.as_deref() == Some("start")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Draft {
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparedProject {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Prepared {
    pub title: String,
    pub initial_prompt: String,
    pub tool: String,
    pub project: Option<PreparedProject>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub yolo: bool,
}

impl DaemonClient {
    fn suggestions_url(&self, path: &str) -> Result<reqwest::Url, DaemonClientError> {
        self.sessions_url
            .join(path)
            .map_err(|_| DaemonClientError::Transport)
    }

    /// Sends `request` with this client's credentials and answers with any
    /// status: suggestion refusals carry typed bodies the caller reads.
    async fn send_suggestion_request(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, DaemonClientError> {
        for (name, value) in &self.credentials {
            request = request.header(name.clone(), value.clone());
        }
        let mut request = request.build().map_err(|_| DaemonClientError::Transport)?;
        if request.timeout().is_none() {
            *request.timeout_mut() = Some(DEFAULT_TIMEOUT);
        }
        self.execute(request, None).await
    }

    /// A non-success answer as an error with its typed code and the daemon's
    /// bounded `message`. A 401 or a finite rejection keeps no body, as in
    /// [`request_response`](DaemonClient::request_response).
    async fn suggestion_refusal(response: reqwest::Response) -> DaemonClientError {
        use super::ApiErrorCode;
        let status = response.status();
        let code = ApiErrorCode::from_headers(status, response.headers(), false);
        let finite = status == StatusCode::UNAUTHORIZED
            || matches!(
                code,
                Some(
                    ApiErrorCode::LifecycleLocked
                        | ApiErrorCode::TrashIncomplete
                        | ApiErrorCode::LifecycleSuperseded
                        | ApiErrorCode::ManagedResumeRefused(_)
                        | ApiErrorCode::SandboxMigrationDeferred
                        | ApiErrorCode::SandboxMigrationFailed
                )
            );
        let message = if finite {
            None
        } else {
            super::decode_json_bounded::<serde_json::Value>(response, MAX_REFUSAL_BYTES)
                .await
                .ok()
                .and_then(|body| body.get("message")?.as_str().map(str::to_owned))
                .map(|message| message.chars().take(MAX_REFUSAL_CHARS).collect())
        };
        DaemonClientError::Status {
            status,
            code,
            body: message.unwrap_or_default(),
            truncated: false,
        }
    }

    async fn suggestion_post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<(), DaemonClientError> {
        let url = self.suggestions_url(path)?;
        let response = self
            .send_suggestion_request(self.http.post(url).json(&body))
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::suggestion_refusal(response).await)
        }
    }

    /// `GET /api/suggestions`.
    pub async fn suggestions(&self) -> Result<Snapshot, DaemonClientError> {
        let url = self.suggestions_url("suggestions")?;
        self.request_json(self.http.get(url)).await
    }

    /// `POST /api/suggestions/refresh`. Asks every provider to discover again;
    /// the new items arrive in a later [`suggestions`](Self::suggestions) read.
    pub async fn refresh_suggestions(
        &self,
        expected_profile: Option<&str>,
    ) -> Result<(), DaemonClientError> {
        self.suggestion_post(
            "suggestions/refresh",
            serde_json::json!({ "expected_profile": expected_profile }),
        )
        .await
    }

    /// `POST /api/suggestions/{dismiss,undismiss}`.
    pub async fn set_suggestion_dismissed(
        &self,
        expected_profile: Option<&str>,
        provider_id: &str,
        id: &str,
        dismissed: bool,
    ) -> Result<(), DaemonClientError> {
        let verb = if dismissed { "dismiss" } else { "undismiss" };
        self.suggestion_post(
            &format!("suggestions/{verb}"),
            serde_json::json!({
                "expected_profile": expected_profile,
                "provider_id": provider_id,
                "id": id,
            }),
        )
        .await
    }

    /// `PATCH /api/suggestions/prefs` with a partial prefs object.
    pub async fn set_suggestion_prefs(
        &self,
        patch: &serde_json::Value,
    ) -> Result<(), DaemonClientError> {
        let url = self.suggestions_url("suggestions/prefs")?;
        let response = self
            .send_suggestion_request(self.http.patch(url).json(patch))
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::suggestion_refusal(response).await)
        }
    }

    /// `POST /api/suggestions/prepare`. The New Session prefill for one item.
    pub async fn prepare_suggestion(
        &self,
        expected_profile: Option<&str>,
        provider_id: &str,
        id: &str,
    ) -> Result<Prepared, DaemonClientError> {
        let url = self.suggestions_url("suggestions/prepare")?;
        let body = serde_json::json!({
            "expected_profile": expected_profile,
            "provider_id": provider_id,
            "id": id,
        });
        let response = self
            .send_suggestion_request(self.http.post(url).json(&body))
            .await?;
        if !response.status().is_success() {
            return Err(Self::suggestion_refusal(response).await);
        }
        super::decode_json(response).await
    }

    fn start_request(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::RequestBuilder, DaemonClientError> {
        let url = self.suggestions_url("suggestions/start")?;
        Ok(self.http.post(url).json(body).timeout(START_TIMEOUT))
    }

    /// `POST /api/suggestions/start`. The status and JSON body of the answer,
    /// which carries the typed outcomes (400, 409 and 422) a caller acts on.
    pub async fn start_suggestion(
        &self,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), DaemonClientError> {
        let response = self
            .send_suggestion_request(self.start_request(body)?)
            .await?;
        let status = response.status();
        if status.is_success()
            || matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY
            )
        {
            let value = super::decode_json::<serde_json::Value>(response)
                .await
                .unwrap_or_default();
            return Ok((status.as_u16(), value));
        }
        Err(Self::suggestion_refusal(response).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_start_waits_as_long_as_a_session_create() {
        let client = DaemonClient::new("http://127.0.0.1:1", None).unwrap();
        let request = client
            .start_request(&serde_json::json!({}))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().path(), "/api/suggestions/start");
        assert_eq!(request.timeout(), Some(&START_TIMEOUT));
    }
}
