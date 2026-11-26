//! Client side of the daemon's suggestions API (`/api/suggestions`): the
//! snapshot and prepare wire types and the [`DaemonClient`] calls that read
//! and act on them.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;

use super::{transport, DaemonClient, DaemonClientError, DEFAULT_TIMEOUT};

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
    pub(crate) profile: Option<String>,
    /// The daemon that served this snapshot; set by the loader, not the wire.
    #[serde(skip)]
    pub(crate) daemon: Option<String>,
    /// The daemon's `pre_trust_agent_folders`; set by the loader, not the wire.
    #[serde(skip)]
    pub(crate) pre_trust: Option<bool>,
    pub(crate) providers: Vec<Provider>,
    pub(crate) items: Vec<Item>,
    #[serde(default)]
    pub(crate) prefs: Prefs,
    /// A featured plugin offers suggestions, as the daemon's index says.
    #[serde(default)]
    pub(crate) setup_offered: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub(crate) show_source_ref: bool,
    /// "Show in the sidebar": the web sidebar group and the TUI Home row.
    pub(crate) sidebar: bool,
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
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) error: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub(crate) enabled: bool,
    /// Unix seconds of the last report.
    #[serde(default)]
    pub(crate) updated_at: Option<i64>,
    #[serde(default)]
    pub(crate) refreshing: bool,
    #[serde(default)]
    pub(crate) checks: Vec<Check>,
}

/// A provider's setup check; `fix` is the plugin's own untrusted advice.
#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) fix: String,
}

/// What Start creates an item with, resolved against its Project.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemStart {
    pub(crate) agent: String,
    pub(crate) view: String,
    #[serde(default)]
    pub(crate) worktree: bool,
    #[serde(default)]
    pub(crate) sandbox: bool,
    #[serde(default)]
    pub(crate) yolo: bool,
    /// The first prompt Start sends; absent when the expanded template is too long.
    #[serde(default)]
    pub(crate) prompt: Option<String>,
}

fn enabled_by_default() -> bool {
    true
}

/// The registered Project an item resolves to.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedProject {
    pub(crate) name: String,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub(crate) provider_id: String,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) reason: String,
    pub(crate) source_url: String,
    pub(crate) source_ref: String,
    pub(crate) preview: String,
    #[serde(default)]
    pub(crate) project: Option<String>,
    #[serde(default)]
    pub(crate) dismissed: bool,
    pub(crate) draft: Draft,
    #[serde(default)]
    pub(crate) started_session: Option<Started>,
    #[serde(default)]
    pub(crate) resolved_project: Option<ResolvedProject>,
    /// The repo the provider links to a Project, as `owner/repo` for GitHub.
    #[serde(default)]
    pub(crate) mapping_key: Option<String>,
    #[serde(default)]
    pub(crate) start: Option<ItemStart>,
    /// The source closed or changed; the item cannot start.
    #[serde(default)]
    pub(crate) stale: bool,
}

/// The live Session an item was started as.
#[derive(Debug, Clone, Deserialize)]
pub struct Started {
    pub(crate) id: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) archived: bool,
    /// `"start"` for a one-click Start; only those may be trashed and started again.
    #[serde(default)]
    pub(crate) started_via: Option<String>,
}

impl Started {
    pub(crate) fn one_click(&self) -> bool {
        self.started_via.as_deref() == Some("start")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Draft {
    pub(crate) prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparedProject {
    pub(crate) path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Prepared {
    pub(crate) title: String,
    pub(crate) initial_prompt: String,
    pub(crate) tool: String,
    pub(crate) project: Option<PreparedProject>,
    /// The rest of the repo's mapped Projects, for a worktree Session.
    #[serde(default)]
    pub(crate) extra_projects: Vec<PreparedProject>,
    #[serde(default)]
    pub(crate) view: Option<String>,
    #[serde(default)]
    pub(crate) worktree: bool,
    #[serde(default)]
    pub(crate) sandbox: bool,
    #[serde(default)]
    pub(crate) yolo: bool,
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
        transport::execute(&self.http, self.unix_path.as_deref(), request).await
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
    use axum::{http::StatusCode as Code, routing::post, Json, Router};

    async fn serve(app: Router) -> (DaemonClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (DaemonClient::new(&base, None).unwrap(), server)
    }

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

    #[tokio::test]
    async fn pre_trust_is_patched_through_the_settings_api() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/api/settings",
            axum::routing::patch(move |Json(body): Json<serde_json::Value>| async move {
                sent.send(body).unwrap();
                Json(serde_json::json!({}))
            }),
        );
        let (client, server) = serve(app).await;
        let patch = serde_json::json!({ "session": { "pre_trust_agent_folders": true } });
        client.patch_settings(&patch).await.unwrap();
        server.abort();
        assert_eq!(received.recv().await.unwrap(), patch);
    }

    #[tokio::test]
    async fn the_home_group_pref_is_patched_through_the_suggestions_api() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/api/suggestions/prefs",
            axum::routing::patch(move |Json(body): Json<serde_json::Value>| async move {
                sent.send(body).unwrap();
                Json(serde_json::json!({}))
            }),
        );
        let (client, server) = serve(app).await;
        let patch = serde_json::json!({ "sidebar": true });
        client.set_suggestion_prefs(&patch).await.unwrap();
        server.abort();
        assert_eq!(received.recv().await.unwrap(), patch);
    }

    #[tokio::test]
    async fn item_changes_name_the_profile_they_were_chosen_under() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let record = |sent: tokio::sync::mpsc::UnboundedSender<serde_json::Value>| {
            post(move |Json(body): Json<serde_json::Value>| async move {
                sent.send(body).unwrap();
                Json(serde_json::json!({ "title": "T", "initial_prompt": "", "tool": "claude" }))
            })
        };
        let app = Router::new()
            .route("/api/suggestions/refresh", record(sent.clone()))
            .route("/api/suggestions/dismiss", record(sent.clone()))
            .route("/api/suggestions/prepare", record(sent));
        let (client, server) = serve(app).await;
        client.refresh_suggestions(Some("work")).await.unwrap();
        client
            .set_suggestion_dismissed(Some("work"), "gh", "1", true)
            .await
            .unwrap();
        client
            .prepare_suggestion(Some("work"), "gh", "1")
            .await
            .unwrap();
        server.abort();
        for _ in 0..3 {
            assert_eq!(received.recv().await.unwrap()["expected_profile"], "work");
        }
    }

    #[tokio::test]
    async fn a_start_returns_the_typed_outcome_of_a_409_and_posts_the_approval() {
        let app = Router::new().route(
            "/api/suggestions/start",
            post(|Json(body): Json<serde_json::Value>| async move {
                (
                    Code::CONFLICT,
                    Json(serde_json::json!({ "error": "hooks_need_trust", "echo": body })),
                )
            }),
        );
        let (client, server) = serve(app).await;
        let body = serde_json::json!({ "provider_id": "gh", "id": "1", "approve_hooks": true });
        let (status, answer) = client.start_suggestion(&body).await.unwrap();
        assert_eq!(status, 409);
        assert_eq!(answer["error"], "hooks_need_trust");
        assert_eq!(answer["echo"], body);
        server.abort();
    }

    #[tokio::test]
    async fn a_prepare_refusal_keeps_the_daemons_explanation_or_its_typed_code() {
        let explained = Router::new().route(
            "/api/suggestions/prepare",
            post(|| async {
                (
                    Code::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "validation_failed",
                        "message": "The expanded prompt exceeds 10 bytes"
                    })),
                )
            }),
        );
        let coded = Router::new().route(
            "/api/suggestions/prepare",
            post(|| async {
                (
                    Code::INTERNAL_SERVER_ERROR,
                    [(super::super::ERROR_CODE_HEADER, "start_failed")],
                    Json(serde_json::json!({ "error": "start_failed" })),
                )
            }),
        );
        let plain = Router::new().route(
            "/api/suggestions/prepare",
            post(|| async { (Code::BAD_GATEWAY, "upstream down") }),
        );
        for (app, status, code, expected) in [
            (
                explained,
                StatusCode::BAD_REQUEST,
                None,
                "The expanded prompt exceeds 10 bytes",
            ),
            (
                coded,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(super::super::ApiErrorCode::StartFailed),
                "",
            ),
            (plain, StatusCode::BAD_GATEWAY, None, ""),
        ] {
            let (client, server) = serve(app).await;
            let error = client
                .prepare_suggestion(None, "gh", "1")
                .await
                .unwrap_err();
            server.abort();
            assert!(
                matches!(&error, DaemonClientError::Status { status: s, code: c, body, .. }
                    if *s == status && *c == code && body == expected),
                "{error:?}"
            );
        }
    }
}
