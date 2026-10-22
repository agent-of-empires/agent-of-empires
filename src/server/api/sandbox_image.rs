//! Sandbox image update status, pulls, and dismissal for the Web dashboard. Status is cached
//! per image so the banner and the wizard share one registry request; a pull runs detached, so
//! it keeps going when the page that started it closes.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{api_error, cityhall_block, read_only_block};
use crate::containers::image_update::{self, ImageCheck};
use crate::server::AppState;

/// Long enough that the banner and the wizard share a registry request, short enough that a
/// newly published image shows up within the hour.
const STATUS_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ImageState {
    Available { remote_digest: String },
    Current,
    Unknown { reason: &'static str },
    CheckFailed { error: String },
}

impl ImageState {
    fn from_check(result: anyhow::Result<ImageCheck>) -> Self {
        match result {
            Ok(ImageCheck::Available(update)) => ImageState::Available {
                remote_digest: update.remote_digest,
            },
            Ok(ImageCheck::Current) => ImageState::Current,
            Ok(ImageCheck::Unknown(reason)) => ImageState::Unknown {
                reason: reason.code(),
            },
            Err(error) => ImageState::CheckFailed {
                error: format!("{error:#}"),
            },
        }
    }
}

/// A dismissal sticks only while the registry still serves the dismissed digest.
fn is_dismissed(state: &ImageState, dismissed_digest: Option<&str>) -> bool {
    matches!(state, ImageState::Available { remote_digest }
        if dismissed_digest == Some(remote_digest.as_str()))
}

#[derive(Default)]
struct StatusCache {
    entries: HashMap<String, (Instant, ImageState)>,
}

impl StatusCache {
    /// A result checked after `not_before` (a fresh request) or within the TTL.
    fn get(&self, image: &str, now: Instant, not_before: Option<Instant>) -> Option<ImageState> {
        let (at, state) = self.entries.get(image)?;
        let usable = match not_before {
            Some(not_before) => *at >= not_before,
            None => now.duration_since(*at) < STATUS_TTL,
        };
        usable.then(|| state.clone())
    }

    fn store(&mut self, image: &str, at: Instant, state: ImageState) {
        self.entries.insert(image.to_string(), (at, state));
    }

    fn invalidate(&mut self, image: &str) {
        self.entries.remove(image);
    }
}

static STATUS: LazyLock<Mutex<StatusCache>> = LazyLock::new(Mutex::default);
/// One registry check per image at a time, so callers arriving together share it.
static CHECK_TURNS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

async fn image_state(image: &str, fresh: bool) -> ImageState {
    let requested_at = Instant::now();
    let turn = CHECK_TURNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(image.to_string())
        .or_default()
        .clone();
    let image = image.to_string();
    // Detached so a closed page still caches the answer for the next caller.
    tokio::spawn(async move {
        let _turn = turn.lock().await;
        let not_before = fresh.then_some(requested_at);
        let cached = STATUS.lock().unwrap_or_else(|e| e.into_inner()).get(
            &image,
            Instant::now(),
            not_before,
        );
        if let Some(state) = cached {
            return state;
        }
        let state = ImageState::from_check(image_update::check_image(&image).await);
        STATUS.lock().unwrap_or_else(|e| e.into_inner()).store(
            &image,
            Instant::now(),
            state.clone(),
        );
        state
    })
    .await
    .unwrap_or_else(|e| ImageState::CheckFailed {
        error: format!("image check task failed: {e}"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PullState {
    Idle,
    Running,
    Done,
    /// `error` carries the Container runtime's stderr.
    Failed {
        error: String,
    },
}

static PULLS: LazyLock<Mutex<HashMap<String, PullState>>> = LazyLock::new(Mutex::default);

/// Marks `image` running unless it already is; true when the caller should start the pull.
fn claim_pull(pulls: &mut HashMap<String, PullState>, image: &str) -> bool {
    if pulls.get(image) == Some(&PullState::Running) {
        return false;
    }
    pulls.insert(image.to_string(), PullState::Running);
    true
}

fn pull_state(image: &str) -> PullState {
    PULLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(image)
        .cloned()
        .unwrap_or(PullState::Idle)
}

/// What the Daemon would sandbox with. Only these are checked or pulled, so a request can't
/// make the Daemon contact an arbitrary registry or pull an arbitrary image.
struct Configured {
    default_image: String,
    images: Vec<String>,
    checks_enabled: bool,
    enabled_by_default: bool,
    allow_image_pull: bool,
    dismissed_digest: Option<String>,
}

fn load_configured() -> Configured {
    let config = crate::session::Config::load_or_warn();
    let default_image = config.sandbox.default_image.clone();
    let mut images = vec![default_image.clone()];
    for profile in crate::session::list_profiles().unwrap_or_default() {
        let image = crate::session::resolve_config_or_warn(&profile)
            .sandbox
            .default_image;
        if !images.contains(&image) {
            images.push(image);
        }
    }
    Configured {
        default_image,
        images,
        checks_enabled: config.updates.update_check_mode.is_enabled(),
        enabled_by_default: config.sandbox.enabled_by_default,
        allow_image_pull: config.sandbox.allow_image_pull,
        dismissed_digest: config.app_state.dismissed_image_digest,
    }
}

async fn configured() -> Result<Configured, Response> {
    tokio::task::spawn_blocking(load_configured)
        .await
        .map_err(|e| {
            tracing::error!(target: "http.api.sandbox_image", "loading config panicked: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        })
}

fn resolve_image(configured: &Configured, requested: Option<&str>) -> Result<String, Response> {
    match requested {
        None => Ok(configured.default_image.clone()),
        Some(image) if configured.images.iter().any(|i| i == image) => Ok(image.to_string()),
        Some(_) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "unknown_image",
            "Only a configured sandbox image can be checked or pulled",
        )),
    }
}

/// Why the dashboard can't pull, or None when it can.
fn pull_blocked(state: &AppState, configured: &Configured) -> Option<&'static str> {
    if state.read_only {
        Some("read_only")
    } else if state.cityhall_mode {
        Some("cityhall")
    } else if !configured.allow_image_pull {
        Some("disabled")
    } else {
        None
    }
}

#[derive(Deserialize)]
pub struct ImageStatusQuery {
    image: Option<String>,
    /// Skip the cache, as the wizard does when it opens.
    #[serde(default)]
    fresh: bool,
}

#[derive(Serialize)]
pub struct ImageStatusResponse {
    image: String,
    #[serde(flatten)]
    state: ImageState,
    dismissed: bool,
    /// False when update checks are off; the state is then `unknown` and no registry is asked.
    checks_enabled: bool,
    /// `sandbox.enabled_by_default`, or a sandboxed Session exists. The banner shows only then.
    sandbox_in_use: bool,
    pull: PullState,
    /// `read_only`, `cityhall` or `disabled` when the dashboard can't pull; `pull_command` is
    /// then what to run instead.
    pull_blocked: Option<&'static str>,
    pull_command: String,
    runtime: &'static str,
}

/// `GET /api/sandbox/image-status?image=<ref>&fresh=true`
pub async fn sandbox_image_status(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ImageStatusQuery>,
) -> Response {
    let configured = match configured().await {
        Ok(configured) => configured,
        Err(resp) => return resp,
    };
    let image = match resolve_image(&configured, query.image.as_deref()) {
        Ok(image) => image,
        Err(resp) => return resp,
    };
    let image_state = if configured.checks_enabled {
        image_state(&image, query.fresh).await
    } else {
        ImageState::Unknown {
            reason: "checks_off",
        }
    };
    let dismissed = is_dismissed(&image_state, configured.dismissed_digest.as_deref());
    let sandbox_in_use = configured.enabled_by_default
        || state
            .instances
            .read()
            .await
            .iter()
            .any(|i| i.is_sandboxed());
    let runtime = crate::containers::get_container_runtime();
    Json(ImageStatusResponse {
        pull: pull_state(&image),
        pull_blocked: pull_blocked(&state, &configured),
        pull_command: runtime.pull_command(&image),
        runtime: runtime.display_name(),
        image,
        state: image_state,
        dismissed,
        checks_enabled: configured.checks_enabled,
        sandbox_in_use,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct ImagePullBody {
    image: Option<String>,
}

/// `POST /api/sandbox/image-pull`: starts at most one pull per image and returns at once;
/// `image-status` reports its progress. The pull keeps its 600 s runtime timeout.
pub async fn sandbox_image_pull(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ImagePullBody>, JsonRejection>,
) -> Response {
    if let Some(resp) = read_only_block(&state).or_else(|| cityhall_block(&state)) {
        return resp;
    }
    let configured = match configured().await {
        Ok(configured) => configured,
        Err(resp) => return resp,
    };
    if pull_blocked(&state, &configured).is_some() {
        return api_error(
            StatusCode::FORBIDDEN,
            "pull_disabled",
            "Pulling sandbox images from the web is off. Enable sandbox.allow_image_pull (Settings, local only).",
        );
    }
    // No body pulls the default image; a body that isn't valid JSON is an error.
    let requested = match body {
        Ok(Json(body)) => body.image,
        Err(JsonRejection::MissingJsonContentType(_)) => None,
        Err(rejection) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                rejection.body_text(),
            )
        }
    };
    let image = match resolve_image(&configured, requested.as_deref()) {
        Ok(image) => image,
        Err(resp) => return resp,
    };
    let started = claim_pull(&mut PULLS.lock().unwrap_or_else(|e| e.into_inner()), &image);
    if started {
        let job_image = image.clone();
        tokio::spawn(async move {
            let pulled = job_image.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::containers::get_container_runtime().pull_image(&pulled)
            })
            .await;
            let outcome = match result {
                Ok(Ok(())) => PullState::Done,
                Ok(Err(e)) => PullState::Failed {
                    error: e.to_string(),
                },
                Err(e) => PullState::Failed {
                    error: format!("pull task failed: {e}"),
                },
            };
            STATUS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .invalidate(&job_image);
            PULLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(job_image, outcome);
        });
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "image": image, "pull": PullState::Running })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ImageDismissBody {
    digest: String,
}

/// `POST /api/sandbox/image-dismiss`: snoozes the banner for this remote digest. The TUI reads
/// and writes the same `app_state.dismissed_image_digest`.
pub async fn sandbox_image_dismiss(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ImageDismissBody>, JsonRejection>,
) -> Response {
    if let Some(resp) = read_only_block(&state).or_else(|| cityhall_block(&state)) {
        return resp;
    }
    let digest = match body {
        Ok(Json(body)) if body.digest.starts_with("sha256:") && body.digest.len() <= 80 => {
            body.digest
        }
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_digest",
                "Expected a sha256 image digest",
            )
        }
    };
    let stored = digest.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::session::update_app_state(|app| app.dismissed_image_digest = Some(stored))
    })
    .await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({ "dismissed_image_digest": digest })).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.sandbox_image", "dismissing image update failed: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "Failed to persist the dismissal",
            )
        }
        Err(e) => {
            tracing::error!(target: "http.api.sandbox_image", "dismissing image update panicked: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal server error",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::image_update::{ImageUpdate, UnknownReason};
    use axum::body::to_bytes;

    async fn json(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[test]
    fn each_check_result_keeps_its_own_state() {
        let cases = [
            (
                Ok(ImageCheck::Available(ImageUpdate {
                    image: "img".into(),
                    remote_digest: "sha256:b".into(),
                })),
                serde_json::json!({"state": "available", "remote_digest": "sha256:b"}),
            ),
            (
                Ok(ImageCheck::Current),
                serde_json::json!({"state": "current"}),
            ),
            (
                Ok(ImageCheck::Unknown(UnknownReason::Pinned)),
                serde_json::json!({"state": "unknown", "reason": "pinned"}),
            ),
            (
                Ok(ImageCheck::Unknown(UnknownReason::NotPulled)),
                serde_json::json!({"state": "unknown", "reason": "not_pulled"}),
            ),
            (
                Ok(ImageCheck::Unknown(UnknownReason::NoLocalDigest)),
                serde_json::json!({"state": "unknown", "reason": "no_local_digest"}),
            ),
            // A registry or network error is never reported as current.
            (
                Err(anyhow::anyhow!("registry timed out")),
                serde_json::json!({"state": "check_failed", "error": "registry timed out"}),
            ),
        ];
        for (result, expected) in cases {
            let state = ImageState::from_check(result);
            assert_eq!(serde_json::to_value(&state).unwrap(), expected);
        }

        let available = |digest: &str| ImageState::Available {
            remote_digest: digest.into(),
        };
        assert!(is_dismissed(&available("sha256:b"), Some("sha256:b")));
        assert!(
            !is_dismissed(&available("sha256:c"), Some("sha256:b")),
            "registry moved"
        );
        assert!(!is_dismissed(&ImageState::Current, Some("sha256:b")));
    }

    #[test]
    fn the_cache_serves_within_its_ttl_honours_fresh_and_drops_a_pulled_image() {
        let mut cache = StatusCache::default();
        let checked = Instant::now();
        cache.store("img", checked, ImageState::Current);

        let within = checked + STATUS_TTL - Duration::from_secs(1);
        assert_eq!(cache.get("img", within, None), Some(ImageState::Current));
        assert_eq!(
            cache.get("img", checked + STATUS_TTL, None),
            None,
            "expired"
        );
        // A fresh request only takes a result checked after it arrived.
        assert_eq!(
            cache.get("img", within, Some(checked + Duration::from_secs(1))),
            None
        );
        assert_eq!(
            cache.get("img", within, Some(checked)),
            Some(ImageState::Current)
        );

        cache.invalidate("img");
        assert_eq!(cache.get("img", checked, None), None, "a pull invalidates");
    }

    #[test]
    fn only_one_pull_per_image_runs_at_a_time() {
        let mut pulls = HashMap::new();
        assert!(claim_pull(&mut pulls, "img"));
        assert!(!claim_pull(&mut pulls, "img"), "already running");
        assert!(claim_pull(&mut pulls, "other"), "per image");
        for finished in [
            PullState::Done,
            PullState::Failed {
                error: "denied".into(),
            },
        ] {
            pulls.insert("img".into(), finished);
            assert!(
                claim_pull(&mut pulls, "img"),
                "a finished pull can be retried"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn status_gate_refusals_and_dismissal_without_contacting_a_registry() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|c| {
            c.updates.update_check_mode = crate::session::config::UpdateCheckMode::Off;
            c.sandbox.allow_image_pull = false;
        })
        .unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let status = |image: Option<&str>| {
            sandbox_image_status(
                State(state.clone()),
                Query(ImageStatusQuery {
                    image: image.map(String::from),
                    fresh: false,
                }),
            )
        };

        let (code, body) = json(status(None).await).await;
        assert_eq!(code, StatusCode::OK);
        let default_image = crate::session::Config::load_or_warn().sandbox.default_image;
        assert_eq!(body["image"], default_image.as_str());
        // Checks off: no registry request, and never "current".
        assert_eq!(body["state"], "unknown");
        assert_eq!(body["reason"], "checks_off");
        assert_eq!(body["checks_enabled"], false);
        assert_eq!(body["sandbox_in_use"], false);
        assert_eq!(body["pull_blocked"], "disabled");
        assert!(
            body["pull_command"]
                .as_str()
                .unwrap()
                .ends_with(&format!(" {default_image}")),
            "{body}"
        );

        let (code, _) = json(status(Some("evil.example/x:latest")).await).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "only configured images");

        let pull = |state: Arc<AppState>| {
            sandbox_image_pull(State(state), Ok(Json(ImagePullBody { image: None })))
        };
        let (code, body) = json(pull(state.clone()).await).await;
        assert_eq!(
            (code, body["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("pull_disabled"))
        );

        let mut read_only = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut read_only).unwrap().read_only = true;
        let (code, body) = json(pull(read_only.clone()).await).await;
        assert_eq!(
            (code, body["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("read_only"))
        );

        let dismiss = |state: Arc<AppState>, digest: &str| {
            sandbox_image_dismiss(
                State(state),
                Ok(Json(ImageDismissBody {
                    digest: digest.into(),
                })),
            )
        };
        let (code, _) = json(dismiss(read_only, "sha256:abc").await).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, _) = json(dismiss(state.clone(), "not-a-digest").await).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, _) = json(dismiss(state.clone(), "sha256:abc").await).await;
        assert_eq!(code, StatusCode::OK);
        // The TUI reads the same field.
        assert_eq!(
            crate::session::Config::load_or_warn()
                .app_state
                .dismissed_image_digest
                .as_deref(),
            Some("sha256:abc")
        );
    }
}
