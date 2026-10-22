//! Sandbox image update status, pulls, and dismissal for the Web dashboard. Status is cached
//! per image so the banner and the wizard share one registry request; a pull runs detached, so
//! it keeps going when the page that started it closes.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
// Tokio's, so tests can drive overlapping checks on a paused clock.
use tokio::time::Instant;

use super::{api_error, cityhall_block, read_only_block};
use crate::containers::container_runtime_for;
use crate::containers::image_update::{self, ImageCheck};
use crate::server::AppState;
use crate::session::ContainerRuntimeName;

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

/// One image as one Container runtime sees it, so switching runtimes can't reuse another's answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImageKey {
    runtime: &'static str,
    image: String,
}

#[derive(Default)]
struct StatusCache {
    entries: HashMap<ImageKey, (Instant, ImageState)>,
    /// Bumped by a pull, so a check that began before it can't store its older answer.
    generations: HashMap<ImageKey, u64>,
}

impl StatusCache {
    /// A result whose check started after `not_before` (a fresh request), or one within the TTL.
    fn get(&self, key: &ImageKey, now: Instant, not_before: Option<Instant>) -> Option<ImageState> {
        let (started, state) = self.entries.get(key)?;
        let usable = match not_before {
            Some(not_before) => *started >= not_before,
            None => now.duration_since(*started) < STATUS_TTL,
        };
        usable.then(|| state.clone())
    }

    fn generation(&self, key: &ImageKey) -> u64 {
        self.generations.get(key).copied().unwrap_or(0)
    }

    /// Keeps a check that started at `started` in `generation`, unless a pull has run since.
    fn store(&mut self, key: &ImageKey, started: Instant, generation: u64, state: ImageState) {
        if self.generation(key) == generation {
            self.entries.insert(key.clone(), (started, state));
        }
    }

    fn invalidate(&mut self, key: &ImageKey) {
        self.entries.remove(key);
        *self.generations.entry(key.clone()).or_default() += 1;
    }
}

static STATUS: LazyLock<Mutex<StatusCache>> = LazyLock::new(Mutex::default);
/// One registry check per image and runtime at a time, so callers arriving together share it.
static CHECK_TURNS: LazyLock<Mutex<HashMap<ImageKey, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

async fn image_state(runtime: ContainerRuntimeName, key: ImageKey, fresh: bool) -> ImageState {
    image_state_with(key, fresh, move |image| async move {
        image_update::check_image(container_runtime_for(runtime), &image).await
    })
    .await
}

async fn image_state_with<F, Fut>(key: ImageKey, fresh: bool, check: F) -> ImageState
where
    F: FnOnce(String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<ImageCheck>> + Send,
{
    let requested_at = Instant::now();
    let turn = CHECK_TURNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.clone())
        .or_default()
        .clone();
    // Detached so a closed page still caches the answer for the next caller.
    tokio::spawn(async move {
        let _turn = turn.lock().await;
        let (cached, generation) = {
            let cache = STATUS.lock().unwrap_or_else(|e| e.into_inner());
            let not_before = fresh.then_some(requested_at);
            (
                cache.get(&key, Instant::now(), not_before),
                cache.generation(&key),
            )
        };
        if let Some(state) = cached {
            return state;
        }
        let started = Instant::now();
        let state = ImageState::from_check(check(key.image.clone()).await);
        STATUS.lock().unwrap_or_else(|e| e.into_inner()).store(
            &key,
            started,
            generation,
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

static PULLS: LazyLock<Mutex<HashMap<ImageKey, PullState>>> = LazyLock::new(Mutex::default);

/// Marks the image running unless it already is; true when the caller should start the pull.
fn claim_pull(pulls: &mut HashMap<ImageKey, PullState>, key: &ImageKey) -> bool {
    if pulls.get(key) == Some(&PullState::Running) {
        return false;
    }
    pulls.insert(key.clone(), PullState::Running);
    true
}

fn pull_state(key: &ImageKey) -> PullState {
    PULLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
        .unwrap_or(PullState::Idle)
}

/// What the Daemon would sandbox with. Only these are checked or pulled, so a request can't
/// make the Daemon contact an arbitrary registry or pull an arbitrary image.
struct Configured {
    default_image: String,
    images: Vec<String>,
    runtime: ContainerRuntimeName,
    checks_enabled: bool,
    enabled_by_default: bool,
    allow_image_pull: bool,
    dismissed_digest: Option<String>,
}

impl Configured {
    fn key(&self, image: String) -> ImageKey {
        ImageKey {
            runtime: container_runtime_for(self.runtime).display_name(),
            image,
        }
    }
}

/// Strict, so an unreadable config refuses the request rather than falling back to defaults,
/// whose pull gate is open.
fn load_configured() -> anyhow::Result<Configured> {
    let config = crate::session::Config::load()?;
    let default_image = config.sandbox.default_image.clone();
    let mut images = vec![default_image.clone()];
    for profile in crate::session::list_profiles()? {
        match crate::session::resolve_config(&profile) {
            Ok(resolved) if !images.contains(&resolved.sandbox.default_image) => {
                images.push(resolved.sandbox.default_image);
            }
            Ok(_) => {}
            // An unreadable profile only drops its image from what may be checked or pulled.
            Err(e) => {
                tracing::warn!(target: "http.api.sandbox_image", %profile, "skipping profile: {e:#}")
            }
        }
    }
    Ok(Configured {
        default_image,
        images,
        runtime: config.sandbox.container_runtime,
        checks_enabled: config.updates.update_check_mode.is_enabled(),
        enabled_by_default: config.sandbox.enabled_by_default,
        allow_image_pull: config.sandbox.allow_image_pull,
        dismissed_digest: config.app_state.dismissed_image_digest,
    })
}

async fn configured() -> Result<Configured, Response> {
    let loaded = tokio::task::spawn_blocking(load_configured)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("loading config panicked: {e}")));
    loaded.map_err(|e| {
        tracing::error!(target: "http.api.sandbox_image", "reading the configuration failed: {e:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_unreadable",
            "Couldn't read the AoE configuration, so no sandbox image is checked or pulled",
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
    let runtime = container_runtime_for(configured.runtime);
    let key = configured.key(image.clone());
    let image_state = if configured.checks_enabled {
        image_state(configured.runtime, key.clone(), query.fresh).await
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
    Json(ImageStatusResponse {
        pull: pull_state(&key),
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

/// No body pulls the default image; any other body must be a JSON `{image}`. A `Json` extractor
/// can't tell an empty body from one sent without a JSON content type.
fn requested_image(
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Option<String>, Response> {
    if body.is_empty() {
        return Ok(None);
    }
    // Media types are case-insensitive, and parameters such as `charset` follow a `;`.
    let is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"));
    // A body that names no image is malformed, not a request for the default.
    match serde_json::from_slice::<ImagePullBody>(body) {
        Ok(ImagePullBody { image: Some(image) }) if is_json => Ok(Some(image)),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "Expected no body, or a JSON body like {\"image\": \"<configured image>\"}",
        )),
    }
}

/// `POST /api/sandbox/image-pull`: starts at most one pull per image and returns at once;
/// `image-status` reports its progress. The pull keeps its 600 s runtime timeout.
pub async fn sandbox_image_pull(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
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
    let requested = match requested_image(&headers, &body) {
        Ok(requested) => requested,
        Err(resp) => return resp,
    };
    let image = match resolve_image(&configured, requested.as_deref()) {
        Ok(image) => image,
        Err(resp) => return resp,
    };
    let key = configured.key(image.clone());
    let started = claim_pull(&mut PULLS.lock().unwrap_or_else(|e| e.into_inner()), &key);
    if started {
        let runtime = configured.runtime;
        tokio::spawn(async move {
            let pulled = key.image.clone();
            let result = tokio::task::spawn_blocking(move || {
                container_runtime_for(runtime).pull_image(&pulled)
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
                .invalidate(&key);
            PULLS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, outcome);
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

    fn key(runtime: &'static str, image: &str) -> ImageKey {
        ImageKey {
            runtime,
            image: image.into(),
        }
    }

    #[test]
    fn the_cache_is_dated_by_check_start_and_discards_a_check_a_pull_overtook() {
        let docker = key("Docker", "img");
        let mut cache = StatusCache::default();
        let started = Instant::now();
        cache.store(
            &docker,
            started,
            cache.generation(&docker),
            ImageState::Current,
        );

        let within = started + STATUS_TTL - Duration::from_secs(1);
        assert_eq!(cache.get(&docker, within, None), Some(ImageState::Current));
        assert_eq!(
            cache.get(&docker, started + STATUS_TTL, None),
            None,
            "expired"
        );

        // A check that started before a fresh request doesn't satisfy it, however late it ended.
        assert_eq!(
            cache.get(&docker, within, Some(started + Duration::from_secs(1))),
            None
        );
        assert_eq!(
            cache.get(&docker, within, Some(started)),
            Some(ImageState::Current)
        );

        // A check in flight when a pull finishes can't store its pre-pull answer.
        let generation = cache.generation(&docker);
        cache.invalidate(&docker);
        assert_eq!(
            cache.get(&docker, started, None),
            None,
            "a pull invalidates"
        );
        let available = ImageState::Available {
            remote_digest: "sha256:old".into(),
        };
        cache.store(&docker, Instant::now(), generation, available);
        assert_eq!(
            cache.get(&docker, Instant::now(), None),
            None,
            "stale check discarded"
        );
        let current = cache.generation(&docker);
        cache.store(&docker, Instant::now(), current, ImageState::Current);
        assert_eq!(
            cache.get(&docker, Instant::now(), None),
            Some(ImageState::Current)
        );
    }

    fn available(digest: &str) -> ImageState {
        ImageState::Available {
            remote_digest: digest.into(),
        }
    }

    /// A 10 s check that answers with how many checks have run, so each answer is traceable.
    fn slow_check(
        checks: Arc<std::sync::atomic::AtomicUsize>,
    ) -> impl FnOnce(
        String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<ImageCheck>> + Send>,
    > {
        move |image| {
            Box::pin(async move {
                let n = checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(ImageCheck::Available(ImageUpdate {
                    image,
                    remote_digest: format!("sha256:{n}"),
                }))
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_requests_and_a_pull_mid_check_never_get_an_older_answer() {
        let key = key("Docker", "overlap-test-image");
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // A starts a check; B asks for a fresh answer while it runs.
        let a = tokio::spawn(image_state_with(
            key.clone(),
            false,
            slow_check(checks.clone()),
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;
        let b = tokio::spawn(image_state_with(
            key.clone(),
            true,
            slow_check(checks.clone()),
        ));
        assert_eq!(a.await.unwrap(), available("sha256:1"));
        // A's check started before B arrived, so it can't count as fresh however late it ended.
        assert_eq!(b.await.unwrap(), available("sha256:2"));
        // A cached request shares the newest answer.
        let cached = image_state_with(key.clone(), false, slow_check(checks.clone())).await;
        assert_eq!(cached, available("sha256:2"));

        // A pull finishing mid-check: that check still answers its caller, but isn't cached.
        let c = tokio::spawn(image_state_with(
            key.clone(),
            true,
            slow_check(checks.clone()),
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;
        STATUS.lock().unwrap().invalidate(&key);
        assert_eq!(c.await.unwrap(), available("sha256:3"));
        let after = image_state_with(key.clone(), false, slow_check(checks.clone())).await;
        assert_eq!(
            after,
            available("sha256:4"),
            "the pre-pull answer was not served"
        );
    }

    #[test]
    fn a_runtime_switch_keys_status_and_pulls_apart() {
        let configured = |runtime| Configured {
            default_image: "img".into(),
            images: Vec::new(),
            runtime,
            checks_enabled: true,
            enabled_by_default: false,
            allow_image_pull: true,
            dismissed_digest: None,
        };
        let docker = configured(ContainerRuntimeName::Docker).key("img".into());
        let podman = configured(ContainerRuntimeName::Podman).key("img".into());
        assert_ne!(docker, podman);
        let mut cache = StatusCache::default();
        cache.store(&docker, Instant::now(), 0, ImageState::Current);
        assert_eq!(
            cache.get(&podman, Instant::now(), None),
            None,
            "Docker's answer isn't Podman's"
        );
    }

    #[test]
    fn a_pull_body_is_empty_or_json_never_silently_the_default() {
        let json_type = || {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            headers
        };
        let none = axum::http::HeaderMap::new;
        let typed = |value: &str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(axum::http::header::CONTENT_TYPE, value.parse().unwrap());
            headers
        };
        let image = br#"{"image":"ghcr.io/x/y:1"}"#;
        // (headers, body, requested image, or None for invalid_body)
        let cases: [(axum::http::HeaderMap, &[u8], Option<Option<&str>>); 10] = [
            (none(), b"", Some(None)),
            (json_type(), b"", Some(None)),
            (json_type(), image, Some(Some("ghcr.io/x/y:1"))),
            (
                typed("Application/JSON; charset=utf-8"),
                image,
                Some(Some("ghcr.io/x/y:1")),
            ),
            // A configured image sent without a JSON content type must not pull the default.
            (none(), image, None),
            (typed("application/json-extra"), image, None),
            (json_type(), b"{not json", None),
            // A body naming no image is malformed, not a request for the default.
            (json_type(), b"{}", None),
            (json_type(), br#"{"image":null}"#, None),
            (typed("text/plain"), image, None),
        ];
        for (headers, body, want) in cases {
            let got = requested_image(&headers, body);
            match want {
                Some(image) => assert_eq!(got.ok().flatten().as_deref(), image, "{body:?}"),
                None => assert_eq!(
                    got.err().map(|r| r.status()),
                    Some(StatusCode::BAD_REQUEST),
                    "{body:?}"
                ),
            }
        }
    }

    #[test]
    fn only_one_pull_per_image_and_runtime_runs_at_a_time() {
        let mut pulls = HashMap::new();
        let docker = key("Docker", "img");
        assert!(claim_pull(&mut pulls, &docker));
        assert!(!claim_pull(&mut pulls, &docker), "already running");
        assert!(claim_pull(&mut pulls, &key("Docker", "other")), "per image");
        assert!(claim_pull(&mut pulls, &key("Podman", "img")), "per runtime");
        for finished in [
            PullState::Done,
            PullState::Failed {
                error: "denied".into(),
            },
        ] {
            pulls.insert(docker.clone(), finished);
            assert!(
                claim_pull(&mut pulls, &docker),
                "a finished pull can be retried"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_unreadable_configuration_refuses_status_and_pull() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|c| c.sandbox.allow_image_pull = false).unwrap();
        let dir = crate::session::get_app_dir().unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        // (file made unreadable, what it breaks)
        for (file, label) in [
            ("state.toml", "malformed state.toml under a disabled gate"),
            ("config.toml", "malformed config.toml"),
        ] {
            let path = dir.join(file);
            let original = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::write(&path, "not = [valid").unwrap();
            let pull = sandbox_image_pull(
                State(state.clone()),
                axum::http::HeaderMap::new(),
                axum::body::Bytes::new(),
            )
            .await;
            let (code, body) = json(pull).await;
            assert_eq!(
                (code, body["error"].as_str()),
                (StatusCode::INTERNAL_SERVER_ERROR, Some("config_unreadable")),
                "{label}"
            );
            let status = sandbox_image_status(
                State(state.clone()),
                Query(ImageStatusQuery {
                    image: None,
                    fresh: false,
                }),
            )
            .await;
            assert_eq!(
                status.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{label}"
            );
            assert!(
                PULLS
                    .lock()
                    .unwrap()
                    .values()
                    .all(|p| *p != PullState::Running),
                "{label}: no pull started"
            );
            std::fs::write(&path, original).unwrap();
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
            sandbox_image_pull(
                State(state),
                axum::http::HeaderMap::new(),
                axum::body::Bytes::new(),
            )
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
