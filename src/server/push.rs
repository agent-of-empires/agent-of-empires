#![allow(clippy::result_large_err)]
//! Web Push notifications for the dashboard PWA.

use crate::session::Status;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

/// Emitted when an instance's status changes.
#[derive(Clone, Debug)]
pub struct StatusChange {
    pub instance_id: String,
    pub instance_title: String,
    pub old: Status,
    pub new: Status,
    pub at: DateTime<Utc>,
}

/// Capacity of the broadcast channel.
pub const STATUS_CHANNEL_CAPACITY: usize = 64;

/// Dwell requirement for Waiting.
pub const DWELL_WAITING_MS: u64 = 5_000;

/// Dwell for Idle and Error is shorter because these are terminal states and far less
/// flicker-prone.
pub const DWELL_TERMINAL_MS: u64 = 2_000;

/// Post-send cooldown per session.
pub const COOLDOWN_MS: u64 = 60_000;

/// Delay between hitting "Send test notification" and the server actually firing the push.
const TEST_DELAY_MS: u64 = 3_000;

// ── VAPID keypair ───────────────────────────────────────────────────────────

/// Persisted form of the VAPID keypair.
#[derive(Serialize, Deserialize)]
pub struct VapidKeypairFile {
    pub private_pem: String,
    pub public_b64url: String,
    pub created_at: DateTime<Utc>,
}

pub struct VapidKeypair {
    pub signing_key: p256::ecdsa::SigningKey,
    pub public_b64url: String,
    pub private_pem: String,
}

impl VapidKeypair {
    /// Load from disk, or generate and persist a new keypair.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        use fs2::FileExt;
        use std::fs::OpenOptions;

        // Short-circuit: file already present, load directly.
        if path.exists() {
            return Self::load(path);
        }

        // Acquire the generate-lock (creating the lock file if absent).
        let lock_path = path.with_extension("json.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock_exclusive()?;

        // Re-check.
        if path.exists() {
            if let Err(e) = FileExt::unlock(&lock_file) {
                tracing::debug!(target: "http.middleware", "Failed to release lock file: {e}");
            }
            return Self::load(path);
        }

        let kp = Self::generate()?;
        kp.persist(path)?;
        if let Err(e) = FileExt::unlock(&lock_file) {
            tracing::debug!(target: "http.middleware", "Failed to release lock file: {e}");
        }
        Ok(kp)
    }

    fn generate() -> anyhow::Result<Self> {
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::EncodePrivateKey;

        // Pull 32 bytes of OS entropy and reduce via SigningKey::from_slice;
        // avoids the rand/rand_core OsRng shuffle across major versions.
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("getrandom failed: {}", e))?;
        let signing_key = SigningKey::from_slice(&seed)
            .map_err(|e| anyhow::anyhow!("derive signing key: {}", e))?;
        let verifying_key = signing_key.verifying_key();

        // Private key as PKCS#8 PEM.
        let private_pem = signing_key
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)?
            .to_string();

        // Public key in uncompressed SEC1 form, base64url encoded.
        let public_bytes = verifying_key.to_encoded_point(false);
        let public_b64url = base64_url_encode(public_bytes.as_bytes());

        Ok(Self {
            signing_key,
            public_b64url,
            private_pem,
        })
    }

    fn load(path: &Path) -> anyhow::Result<Self> {
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::DecodePrivateKey;

        let raw = std::fs::read_to_string(path)?;
        let file: VapidKeypairFile = serde_json::from_str(&raw)?;
        let signing_key = SigningKey::from_pkcs8_pem(&file.private_pem)?;
        Ok(Self {
            signing_key,
            public_b64url: file.public_b64url,
            private_pem: file.private_pem,
        })
    }

    fn persist(&self, path: &Path) -> anyhow::Result<()> {
        let file = VapidKeypairFile {
            private_pem: self.private_pem.clone(),
            public_b64url: self.public_b64url.clone(),
            created_at: Utc::now(),
        };
        let body = serde_json::to_string_pretty(&file)?;

        // Atomic: write to tmp, fsync, rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ── Subscription store ──────────────────────────────────────────────────────

/// A browser push subscription.
#[derive(Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// SHA-256 of the bearer token at the time of subscribe.
    pub owner_token_hash: [u8; 32],
    pub user_agent: String,
    pub created_at: DateTime<Utc>,
    /// Monotonic counter for optimistic-lock GC.
    pub generation: u64,
    /// Origin (scheme + host + optional port) the subscriber registered from, e.g.
    /// `http://localhost:42041` or `https://aoe.example.com`.
    #[serde(default)]
    pub origin: String,
}

pub struct SubscriptionStore {
    path: PathBuf,
    subs: RwLock<HashMap<String, Subscription>>,
}

impl SubscriptionStore {
    pub fn load_or_empty(path: PathBuf) -> Self {
        let subs = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Vec<Subscription>>(&raw)
                .map(|v| v.into_iter().map(|s| (s.endpoint.clone(), s)).collect())
                .unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            subs: RwLock::new(subs),
        }
    }

    pub async fn snapshot(&self) -> Vec<Subscription> {
        self.subs.read().await.values().cloned().collect()
    }

    pub async fn for_owner(&self, owner: &[u8; 32]) -> Vec<Subscription> {
        self.subs
            .read()
            .await
            .values()
            .filter(|s| &s.owner_token_hash == owner)
            .cloned()
            .collect()
    }

    pub async fn upsert(&self, mut sub: Subscription) -> anyhow::Result<()> {
        {
            let mut guard = self.subs.write().await;
            if let Some(existing) = guard.get(&sub.endpoint) {
                sub.generation = existing.generation.saturating_add(1);
                sub.created_at = existing.created_at;
            }
            guard.insert(sub.endpoint.clone(), sub);
        }
        self.persist().await
    }

    pub async fn remove_if_owner(&self, endpoint: &str, owner: &[u8; 32]) -> anyhow::Result<bool> {
        let removed = {
            let mut guard = self.subs.write().await;
            match guard.get(endpoint) {
                Some(s) if &s.owner_token_hash == owner => {
                    guard.remove(endpoint);
                    true
                }
                _ => false,
            }
        };
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// GC a subscription following a push-endpoint 410/404, gated on the generation counter
    /// so we don't wipe an entry that was re-subscribed while the send was in flight.
    pub async fn gc_stale(&self, endpoint: &str, observed_generation: u64) -> anyhow::Result<bool> {
        let removed = {
            let mut guard = self.subs.write().await;
            match guard.get(endpoint) {
                Some(s) if s.generation == observed_generation => {
                    guard.remove(endpoint);
                    true
                }
                _ => false,
            }
        };
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// Drop any subscriptions whose owner hash is not in `valid`.
    pub async fn retain_owners(&self, valid: &[[u8; 32]]) -> anyhow::Result<usize> {
        let removed = {
            let mut guard = self.subs.write().await;
            let before = guard.len();
            guard.retain(|_, s| valid.iter().any(|v| v == &s.owner_token_hash));
            before - guard.len()
        };
        if removed > 0 {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// Hold the store's lock, stalling every store operation until the guard drops.
    #[cfg(test)]
    pub(crate) async fn hold_for_test(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<String, Subscription>> {
        self.subs.write().await
    }

    async fn persist(&self) -> anyhow::Result<()> {
        let all: Vec<Subscription> = self.subs.read().await.values().cloned().collect();
        let body = serde_json::to_string_pretty(&all)?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &body).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

// ── Module-level state ──────────────────────────────────────────────────────

/// The push feature's mutable state, owned by `AppState.push`.
pub struct PushState {
    pub vapid: VapidKeypair,
    pub store: SubscriptionStore,
    /// VAPID `sub:` claim identifying the sending application.
    pub subject: String,
    /// Shared `SEND_CONCURRENCY` budget across the consumer-driven (`fire_due_pushes`) and
    /// wake-fire (`fire_wake_fired_push`) fan-out paths, so a session with many subscribers
    /// cannot fan out beyond the gateway concurrency the consumer pipeline expects.
    pub send_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
}

/// VAPID `sub` claim (RFC 8292).
pub const VAPID_SUBJECT: &str = "https://github.com/agent-of-empires/agent-of-empires";

impl PushState {
    pub fn init(app_dir: &Path) -> anyhow::Result<Self> {
        let vapid = VapidKeypair::load_or_generate(&app_dir.join("push.vapid.json"))?;
        let store = SubscriptionStore::load_or_empty(app_dir.join("push.subscriptions.json"));
        Ok(Self {
            vapid,
            store,
            subject: VAPID_SUBJECT.to_string(),
            send_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(SEND_CONCURRENCY)),
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn base64_url_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.decode(s)
}

// ── Consumer task ───────────────────────────────────────────────────────────

/// Push-notification event types that the consumer can fire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotificationEvent {
    Waiting,
    Idle,
    Error,
}

impl NotificationEvent {
    fn dwell_ms(self) -> u64 {
        match self {
            Self::Waiting => DWELL_WAITING_MS,
            _ => DWELL_TERMINAL_MS,
        }
    }
}

/// Per-session timing state the consumer maintains to apply the dwell
/// requirement and the post-send cooldown per event type.
#[derive(Default)]
struct DwellState {
    /// When the session most recently entered Waiting.
    waiting_since: Option<std::time::Instant>,
    /// When the session most recently entered Idle.
    idle_since: Option<std::time::Instant>,
    /// When the session most recently entered Error.
    error_since: Option<std::time::Instant>,
    /// Last time a push fired for this session (any event type).
    last_notified: Option<std::time::Instant>,
    /// Cached title for the payload body.
    title: String,
}

/// Max concurrent push sends.
pub const SEND_CONCURRENCY: usize = 8;

/// Spawn the consumer task.
pub fn spawn_consumer(state: std::sync::Arc<super::AppState>) {
    if state.push.is_none() {
        return; // feature disabled, nothing to spawn
    }

    tokio::spawn(async move {
        let client = match super::push_send::build_client() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(target: "http.middleware", error = %e, "push: consumer failed to build reqwest client");
                return;
            }
        };
        let semaphore = state
            .push
            .as_ref()
            .map(|p| p.send_semaphore.clone())
            .expect("spawn_consumer requires push enabled; checked above");
        let mut rx = state.status_tx.subscribe();
        let mut dwell: HashMap<String, DwellState> = HashMap::new();
        // Tracks the last suppression reason so we only log on transitions
        // (active → suppressed, suppressed → active, or reason flip),
        // instead of every 500ms tick while the dashboard is open.
        let mut last_suppress_reason: Option<&'static str> = None;

        // Interleave receiving status changes with polling the dwell map for sessions whose
        // dwell window has elapsed.
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                recv = rx.recv() => {
                    match recv {
                        Ok(change) => handle_status_change(&mut dwell, change),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(target: "http.middleware", lagged = n, "push: consumer lagged, skipped events");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!(target: "http.middleware", "push: status channel closed, consumer exiting");
                            return;
                        }
                    }
                }
                _ = tick.tick() => {
                    fire_due_pushes(state.clone(), &client, &semaphore, &mut dwell, &mut last_suppress_reason).await;
                }
                _ = state.shutdown.cancelled() => {
                    tracing::info!(target: "http.middleware", "push: shutdown signaled, consumer exiting");
                    return;
                }
            }
        }
    });
}

fn handle_status_change(dwell: &mut HashMap<String, DwellState>, change: StatusChange) {
    let entry = dwell.entry(change.instance_id.clone()).or_default();
    entry.title = change.instance_title;
    let now = std::time::Instant::now();
    // Exactly one `*_since` is set at a time.
    entry.waiting_since = None;
    entry.idle_since = None;
    entry.error_since = None;
    match change.new {
        Status::Waiting => entry.waiting_since = Some(now),
        Status::Idle => entry.idle_since = Some(now),
        Status::Error => entry.error_since = Some(now),
        _ => {}
    }
    // Drop entries for transitions into Stopped/Deleting so the map doesn't grow forever in
    // long-running servers that create and destroy many sessions.
    if matches!(change.new, Status::Stopped | Status::Deleting) {
        dwell.remove(&change.instance_id);
    }
}

/// Resolve whether a given event type should fire for a given instance,
/// combining server-wide defaults with per-session overrides.
fn should_fire(
    event: NotificationEvent,
    web: &crate::session::config::WebConfig,
    instance: Option<&crate::session::Instance>,
) -> bool {
    let (global, override_val) = match event {
        NotificationEvent::Waiting => (
            web.notify_on_waiting,
            instance.and_then(|i| i.notify_on_waiting),
        ),
        NotificationEvent::Idle => (web.notify_on_idle, instance.and_then(|i| i.notify_on_idle)),
        NotificationEvent::Error => (
            web.notify_on_error,
            instance.and_then(|i| i.notify_on_error),
        ),
    };
    override_val.unwrap_or(global)
}

/// A status event can sit in the dwell map while a concurrent user action trashes its
/// session.
fn notification_matches_live_instance(
    event: NotificationEvent,
    instance: &crate::session::Instance,
) -> bool {
    if instance.is_trashed() {
        return false;
    }
    matches!(
        (event, instance.status),
        (NotificationEvent::Waiting, Status::Waiting)
            | (NotificationEvent::Idle, Status::Idle)
            | (NotificationEvent::Error, Status::Error)
    )
}

async fn fire_due_pushes(
    app_state: std::sync::Arc<super::AppState>,
    client: &reqwest::Client,
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    dwell: &mut HashMap<String, DwellState>,
    last_suppress_reason: &mut Option<&'static str>,
) {
    let Some(push) = app_state.push.as_ref() else {
        return; // feature disabled, nothing to do
    };
    let push = push.clone();

    // Suppress pushes when the user is actively using aoe (TUI or web dashboard).
    let suppress_reason = if crate::session::is_tui_active(std::time::Duration::from_secs(30)) {
        Some("TUI is active")
    } else if app_state.web_active_within(std::time::Duration::from_secs(30)) {
        Some("web dashboard is active")
    } else {
        None
    };
    // Only log on transitions.
    if suppress_reason != *last_suppress_reason {
        match (*last_suppress_reason, suppress_reason) {
            (None, Some(reason)) => {
                tracing::debug!(target: "http.middleware", "push: suppressed, {}", reason)
            }
            (Some(_), Some(reason)) => {
                tracing::debug!(target: "http.middleware", "push: suppression reason changed to {}", reason)
            }
            (Some(prev), None) => {
                tracing::debug!(target: "http.middleware", "push: resumed (was suppressed: {})", prev)
            }
            (None, None) => {}
        }
        *last_suppress_reason = suppress_reason;
    }
    if suppress_reason.is_some() {
        return;
    }

    let now = std::time::Instant::now();
    // Collect (instance_id, title, event) tuples to fire.
    let mut to_fire: Vec<(String, String, NotificationEvent)> = Vec::new();

    for (id, state) in dwell.iter_mut() {
        // Cooldown gates ALL event types for this session.
        if let Some(last) = state.last_notified {
            if now.duration_since(last).as_millis() < COOLDOWN_MS as u128 {
                continue;
            }
        }

        // Evaluate each event in priority order.
        let checks = [
            (NotificationEvent::Waiting, state.waiting_since),
            (NotificationEvent::Error, state.error_since),
            (NotificationEvent::Idle, state.idle_since),
        ];
        for (event, since_opt) in checks {
            let Some(since) = since_opt else { continue };
            if now.duration_since(since).as_millis() < event.dwell_ms() as u128 {
                continue;
            }
            state.last_notified = Some(now);
            state.waiting_since = None;
            state.idle_since = None;
            state.error_since = None;
            to_fire.push((id.clone(), state.title.clone(), event));
            break;
        }
    }

    if to_fire.is_empty() {
        return;
    }

    // Snapshot instances once; fire_due_pushes holds no locks across
    // the tokio::spawn boundary below.
    let instances = app_state.instances.read().await.clone();
    let web_config = app_state.web_config.clone();

    for (instance_id, instance_title, event) in to_fire {
        // If the instance vanished (externally deleted, tmux killed, storage file
        // hand-edited) between the dwell timer starting and firing, skip rather than
        // sending a notification that deep-links to a 404.
        let Some(instance) = instances.iter().find(|i| i.id == instance_id) else {
            dwell.remove(&instance_id);
            continue;
        };
        if !notification_matches_live_instance(event, instance) {
            continue;
        }
        if !should_fire(event, &web_config, Some(instance)) {
            continue;
        }

        // Acp approval and question pushes are dispatched immediately from
        // `acp_event_listener` with their own tags and bypass the TUI/web active-session
        // suppression.
        if event == NotificationEvent::Waiting
            && (!app_state
                .acp_event_store
                .unresolved_approval_nonces(&instance_id)
                .is_empty()
                || !app_state
                    .acp_event_store
                    .unresolved_elicitation_nonces(&instance_id)
                    .is_empty())
        {
            continue;
        }

        let subs = push.store.snapshot().await;
        if subs.is_empty() {
            continue;
        }

        let (title, body_prefix) = match event {
            NotificationEvent::Waiting => ("Claude is waiting", "Waiting for input"),
            NotificationEvent::Idle => ("Session finished", "Agent is idle"),
            NotificationEvent::Error => ("Session error", "Agent errored"),
        };
        let body = if instance_title.is_empty() {
            body_prefix.to_string()
        } else {
            format!("{}: {}", body_prefix, instance_title)
        };
        let path = format!("/session/{}", instance_id);
        let tag = format!("session-{}", instance_id);

        for sub in subs {
            let Some(url) = build_push_url(&sub, &path) else {
                continue;
            };
            let permit_sem = semaphore.clone();
            let client = client.clone();
            let push = push.clone();
            let payload_clone = super::push_send::PushPayload {
                title: title.to_string(),
                body: body.clone(),
                url,
                tag: tag.clone(),
                session_id: instance_id.clone(),
            };
            tokio::spawn(async move {
                let Ok(_permit) = permit_sem.acquire_owned().await else {
                    return;
                };
                let outcome =
                    super::push_send::send_one(&client, push.as_ref(), &sub, &payload_clone).await;
                if outcome == super::push_send::SendOutcome::Gone {
                    if let Err(e) = push.store.gc_stale(&sub.endpoint, sub.generation).await {
                        tracing::warn!(target: "http.middleware", "Failed to GC stale push subscription: {e}");
                    }
                }
            });
        }
    }
}

/// Fire a one-shot push notification when a structured view session's pending
/// `ScheduleWakeup` actually triggers.
pub async fn fire_wake_fired_push(
    state: std::sync::Arc<super::AppState>,
    session_id: &str,
    session_title: &str,
    reason: Option<&str>,
) {
    let Some(push) = state.push.as_ref().cloned() else {
        return;
    };
    let web_config = state.web_config.clone();
    if !web_config.notifications_enabled || !web_config.notify_on_wake_fire {
        return;
    }
    if crate::session::is_tui_active(std::time::Duration::from_secs(30)) {
        tracing::debug!(
            target: "push.wake_fired",
            session = %session_id,
            "suppressed: TUI is active"
        );
        return;
    }
    if state.web_active_within(std::time::Duration::from_secs(30)) {
        tracing::debug!(
            target: "push.wake_fired",
            session = %session_id,
            "suppressed: web dashboard is active"
        );
        return;
    }

    let subs = push.store.snapshot().await;
    if subs.is_empty() {
        return;
    }
    let client = match super::push_send::build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target: "push.wake_fired",
                "failed to build reqwest client: {e}"
            );
            return;
        }
    };

    let body_suffix = if session_title.is_empty() {
        String::new()
    } else {
        format!(": {}", session_title)
    };
    let body = match reason {
        Some(r) if !r.is_empty() => format!("Agent resumed{}: {}", body_suffix, r),
        _ => format!("Agent resumed{}", body_suffix),
    };
    let path = format!("/session/{}", session_id);
    let tag = format!("session-{}", session_id);

    for sub in subs {
        let Some(url) = build_push_url(&sub, &path) else {
            continue;
        };
        let client = client.clone();
        let push = push.clone();
        let permit_sem = push.send_semaphore.clone();
        let payload_clone = super::push_send::PushPayload {
            title: "Scheduled wakeup fired".to_string(),
            body: body.clone(),
            url,
            tag: tag.clone(),
            session_id: session_id.to_string(),
        };
        tokio::spawn(async move {
            // Acquire from the same SEND_CONCURRENCY budget that `spawn_consumer`'s
            // fire_due_pushes uses, so a wake fire with many subscribers cannot outrun the
            // gateway concurrency cap the rest of the pipeline expects.
            let Ok(_permit) = permit_sem.acquire_owned().await else {
                return;
            };
            let outcome =
                super::push_send::send_one(&client, push.as_ref(), &sub, &payload_clone).await;
            if outcome == super::push_send::SendOutcome::Gone {
                if let Err(e) = push.store.gc_stale(&sub.endpoint, sub.generation).await {
                    tracing::warn!(target: "http.middleware", "Failed to GC stale push subscription: {e}");
                }
            }
        });
    }
}

/// Build an absolute URL for a push payload by joining the subscription's recorded origin
/// with a leading-slash path.
pub fn build_push_url(sub: &Subscription, path: &str) -> Option<String> {
    if sub.origin.is_empty() {
        tracing::info!(
            target: "push",
            endpoint = %sub.endpoint,
            "skipping push: subscription has no origin, ask user to re-subscribe (#1188)"
        );
        return None;
    }
    let origin = sub.origin.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    };
    Some(format!("{origin}{path}"))
}

pub fn sha256_token(token: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

// ── HTTP handlers ───────────────────────────────────────────────────────────

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use std::sync::Arc;

use super::auth::AuthenticatedTokenHash;
use super::AppState;

/// Body accepted by POST /api/push/subscribe.
#[derive(Deserialize)]
pub struct SubscribeBody {
    pub endpoint: String,
    pub keys: SubscribeKeys,
}

#[derive(Deserialize)]
pub struct SubscribeKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Deserialize)]
pub struct EndpointBody {
    pub endpoint: String,
}

#[derive(Serialize)]
pub struct TestResult {
    pub delivered: u32,
    pub failed: u32,
    pub gone: u32,
}

/// GET /api/push/status Tells the client whether the feature is enabled server-wide.
pub async fn get_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "enabled": state.push_enabled }))
}

/// GET /api/push/vapid-public-key Returns the base64url-encoded raw public key for the
/// browser's `pushManager.subscribe({ applicationServerKey })` call.
pub async fn get_vapid_public_key(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let push = state.push.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(
        serde_json::json!({ "public_key": push.vapid.public_b64url }),
    ))
}

/// POST /api/push/subscribe Stores a browser subscription, binding it to the requesting
/// token's hash.
pub async fn subscribe(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthenticatedTokenHash>,
    headers: HeaderMap,
    body: Result<Json<SubscribeBody>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode, axum::response::Response> {
    use axum::response::IntoResponse;
    if state.read_only {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let Json(body) = body.map_err(|rej| rej.into_response())?;
    let push = state
        .push
        .as_ref()
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;

    // Minimal shape validation so we don't store garbage.
    if body.endpoint.is_empty() || body.keys.p256dh.is_empty() || body.keys.auth.is_empty() {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let origin = extract_request_origin(&headers).unwrap_or_default();

    let sub = Subscription {
        endpoint: body.endpoint,
        p256dh: body.keys.p256dh,
        auth: body.keys.auth,
        owner_token_hash: auth.0,
        user_agent,
        created_at: Utc::now(),
        generation: 0,
        origin,
    };
    push.store
        .upsert(sub)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    Ok(StatusCode::NO_CONTENT)
}

/// Extract the client's origin (scheme + host + optional port) from the request headers.
pub fn extract_request_origin(headers: &HeaderMap) -> Option<String> {
    let origin_header = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && *s != "null");
    if let Some(s) = origin_header {
        return Some(s.trim_end_matches('/').to_string());
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("https");
    Some(format!("{scheme}://{host}"))
}

/// POST /api/push/unsubscribe Removes a subscription by endpoint.
pub async fn unsubscribe(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthenticatedTokenHash>,
    body: Result<Json<EndpointBody>, axum::extract::rejection::JsonRejection>,
) -> Result<StatusCode, axum::response::Response> {
    use axum::response::IntoResponse;
    if state.read_only {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let Json(body) = body.map_err(|rej| rej.into_response())?;
    let push = state
        .push
        .as_ref()
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    if body.endpoint.is_empty() {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }
    let removed = push
        .store
        .remove_if_owner(&body.endpoint, &auth.0)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        // Either the endpoint doesn't exist or belongs to another owner.
        Err(StatusCode::FORBIDDEN.into_response())
    }
}

/// POST /api/push/test Fires a single notification to the given endpoint (which MUST belong
/// to the caller).
pub async fn test(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthenticatedTokenHash>,
    body: Result<Json<EndpointBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<TestResult>, axum::response::Response> {
    use axum::response::IntoResponse;
    if state.read_only {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let Json(body) = body.map_err(|rej| rej.into_response())?;
    let push = state
        .push
        .as_ref()
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    if body.endpoint.is_empty() {
        return Err(StatusCode::BAD_REQUEST.into_response());
    }

    // Confirm ownership before doing anything.
    let owned = push
        .store
        .for_owner(&auth.0)
        .await
        .into_iter()
        .find(|s| s.endpoint == body.endpoint);
    let Some(subscription) = owned else {
        return Err(StatusCode::FORBIDDEN.into_response());
    };

    let client = match super::push_send::build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: "http.middleware", error = %e, "push: failed to build reqwest client");
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };

    let Some(url) = build_push_url(&subscription, "/") else {
        // Stale subscription with no recorded origin.
        tracing::info!(
            target: "push",
            endpoint = %subscription.endpoint,
            "test push skipped: subscription has no origin, ask user to re-subscribe (#1188)"
        );
        return Err(StatusCode::CONFLICT.into_response());
    };
    let payload = super::push_send::PushPayload {
        title: "Agent of Empires".to_string(),
        body: "Test notification. If you see this on your lock screen, push is working."
            .to_string(),
        url,
        tag: "aoe-test".to_string(),
        session_id: String::new(),
    };

    tokio::time::sleep(std::time::Duration::from_millis(TEST_DELAY_MS)).await;

    let outcome = super::push_send::send_one(&client, push, &subscription, &payload).await;
    let mut result = TestResult {
        delivered: 0,
        failed: 0,
        gone: 0,
    };
    match outcome {
        super::push_send::SendOutcome::Delivered => result.delivered = 1,
        super::push_send::SendOutcome::Failed => result.failed = 1,
        super::push_send::SendOutcome::Gone => {
            result.gone = 1;
            // Best-effort GC; the result still reports gone=1 even if GC races with a
            // re-subscribe (that's what the generation counter in gc_stale prevents).
            if let Err(e) = push
                .store
                .gc_stale(&body.endpoint, subscription.generation)
                .await
            {
                tracing::warn!(target: "http.middleware", "Failed to GC stale push subscription: {e}");
            }
        }
    }
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vapid_keypair_is_generated_once_and_reloaded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("push.vapid.json");
        let first = VapidKeypair::load_or_generate(&path).unwrap();
        assert!(first.public_b64url.len() > 80);
        assert!(first.private_pem.contains("BEGIN PRIVATE KEY"));
        let second = VapidKeypair::load_or_generate(&path).unwrap();
        assert_eq!(first.public_b64url, second.public_b64url);
        assert_eq!(first.private_pem, second.private_pem);
    }

    /// Re-subscribing bumps the generation, and a GC only removes the
    /// generation it observed failing.
    #[tokio::test]
    async fn subscription_generation_gates_stale_gc() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("push.subscriptions.json");
        let store = SubscriptionStore::load_or_empty(path);

        let sub = Subscription {
            endpoint: "https://push.example/abc".into(),
            p256dh: "pk".into(),
            auth: "auth".into(),
            owner_token_hash: [1u8; 32],
            user_agent: "UA".into(),
            created_at: Utc::now(),
            generation: 0,
            origin: "http://localhost:8080".into(),
        };
        store.upsert(sub.clone()).await.unwrap();
        store.upsert(sub.clone()).await.unwrap();
        let all = store.snapshot().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].generation, 1);

        assert!(!store.gc_stale(&sub.endpoint, 0).await.unwrap());
        assert_eq!(store.snapshot().await.len(), 1);
        assert!(store.gc_stale(&sub.endpoint, 1).await.unwrap());
        assert_eq!(store.snapshot().await.len(), 0);
    }

    #[tokio::test]
    async fn retain_owners_keeps_grace_token_drops_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("push.subscriptions.json");
        let store = SubscriptionStore::load_or_empty(path);

        let mk = |hash: [u8; 32], endpoint: &str| Subscription {
            endpoint: endpoint.to_string(),
            p256dh: "pk".into(),
            auth: "auth".into(),
            owner_token_hash: hash,
            user_agent: "UA".into(),
            created_at: Utc::now(),
            generation: 0,
            origin: "http://localhost:8080".into(),
        };
        store.upsert(mk([1u8; 32], "https://x/1")).await.unwrap();
        store.upsert(mk([2u8; 32], "https://x/2")).await.unwrap();
        store.upsert(mk([3u8; 32], "https://x/3")).await.unwrap();
        assert_eq!(store.snapshot().await.len(), 3);

        // Keep current (hash 2) and grace (hash 1); drop hash 3.
        let removed = store.retain_owners(&[[1u8; 32], [2u8; 32]]).await.unwrap();
        assert_eq!(removed, 1);
        let remaining: Vec<_> = store
            .snapshot()
            .await
            .into_iter()
            .map(|s| s.endpoint)
            .collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&"https://x/1".to_string()));
        assert!(remaining.contains(&"https://x/2".to_string()));

        // After grace expires, only hash 2 remains valid. hash 1 drops.
        let removed = store.retain_owners(&[[2u8; 32]]).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn remove_if_owner_blocks_cross_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("push.subscriptions.json");
        let store = SubscriptionStore::load_or_empty(path);

        let sub = Subscription {
            endpoint: "https://push.example/abc".into(),
            p256dh: "pk".into(),
            auth: "auth".into(),
            owner_token_hash: [1u8; 32],
            user_agent: "UA".into(),
            created_at: Utc::now(),
            generation: 0,
            origin: "http://localhost:8080".into(),
        };
        store.upsert(sub).await.unwrap();

        // Different owner must not succeed.
        let removed = store
            .remove_if_owner("https://push.example/abc", &[2u8; 32])
            .await
            .unwrap();
        assert!(!removed);
        assert_eq!(store.snapshot().await.len(), 1);

        // Correct owner succeeds.
        let removed = store
            .remove_if_owner("https://push.example/abc", &[1u8; 32])
            .await
            .unwrap();
        assert!(removed);
        assert_eq!(store.snapshot().await.len(), 0);
    }

    /// The push URL's origin comes from the Origin header when the browser sent one,
    /// otherwise from the proxy's forwarded scheme plus Host. A `null` Origin (an opaque
    /// context) is no signal, and a forwarded-proto chain names its first hop.
    #[test]
    fn extract_request_origin_prefers_origin_then_forwarded_host() {
        let origin = |headers: &[(&str, &str)]| {
            let mut h = HeaderMap::new();
            for (name, value) in headers {
                h.insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                );
            }
            extract_request_origin(&h)
        };
        let host = ("host", "aoe.example.com");

        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], Option<&'a str>);
        let cases: &[Case] = &[
            (
                "origin wins over host",
                &[
                    ("origin", "http://localhost:42041"),
                    ("host", "ignored.example"),
                ],
                Some("http://localhost:42041"),
            ),
            (
                "trailing slash trimmed",
                &[("origin", "https://aoe.example.com/")],
                Some("https://aoe.example.com"),
            ),
            (
                "null origin falls back to host",
                &[("origin", "null"), host],
                Some("https://aoe.example.com"),
            ),
            (
                "forwarded proto",
                &[("x-forwarded-proto", "https"), host],
                Some("https://aoe.example.com"),
            ),
            (
                "forwarded proto chain takes the first hop",
                &[("x-forwarded-proto", "https, http"), host],
                Some("https://aoe.example.com"),
            ),
            (
                "host alone defaults to https",
                &[host],
                Some("https://aoe.example.com"),
            ),
            ("no signal", &[], None),
        ];
        for (name, headers, want) in cases {
            assert_eq!(origin(headers).as_deref(), *want, "{name}");
        }

        let with_origin = |origin: &str| Subscription {
            endpoint: "https://push.example/abc".into(),
            p256dh: "pk".into(),
            auth: "auth".into(),
            owner_token_hash: [1u8; 32],
            user_agent: "UA".into(),
            created_at: Utc::now(),
            generation: 0,
            origin: origin.into(),
        };
        assert_eq!(
            build_push_url(&with_origin("http://localhost:42041"), "/session/abc").as_deref(),
            Some("http://localhost:42041/session/abc")
        );
        assert_eq!(
            build_push_url(&with_origin("https://aoe.example.com/"), "/").as_deref(),
            Some("https://aoe.example.com/")
        );
        assert_eq!(build_push_url(&with_origin(""), "/session/abc"), None);
    }

    /// Each notifiable status owns one dwell clock: entering a status starts its clock
    /// and stops the others, and leaving the notifiable statuses altogether drops the
    /// entry so a stopped session cannot fire later.
    #[test]
    fn dwell_tracks_one_clock_per_status_and_drops_on_stopped() {
        let mut dwell: HashMap<String, DwellState> = HashMap::new();
        let id = "sess-1".to_string();
        let mut step = |old: Status, new: Status| {
            handle_status_change(
                &mut dwell,
                StatusChange {
                    instance_id: id.clone(),
                    instance_title: "my session".to_string(),
                    old,
                    new,
                    at: Utc::now(),
                },
            );
            dwell.get(&id).map(|s| {
                (
                    s.title.clone(),
                    s.waiting_since.is_some(),
                    s.idle_since.is_some(),
                    s.error_since.is_some(),
                )
            })
        };

        let title = "my session".to_string();
        assert_eq!(
            step(Status::Running, Status::Waiting),
            Some((title.clone(), true, false, false))
        );
        assert_eq!(
            step(Status::Waiting, Status::Error),
            Some((title.clone(), false, false, true))
        );
        assert_eq!(
            step(Status::Error, Status::Idle),
            Some((title.clone(), false, true, false))
        );
        assert_eq!(
            step(Status::Idle, Status::Running),
            Some((title, false, false, false))
        );
        assert_eq!(step(Status::Running, Status::Stopped), None);
    }

    #[test]
    fn should_fire_respects_per_session_override() {
        use crate::session::config::WebConfig;
        use crate::session::Instance;

        let web = WebConfig {
            notifications_enabled: true,
            notify_on_waiting: true,
            notify_on_idle: false, // globally off
            notify_on_error: true,
            notify_on_wake_fire: true,
        };

        // No instance (session not in state): fall back to web defaults.
        assert!(should_fire(NotificationEvent::Waiting, &web, None));
        assert!(!should_fire(NotificationEvent::Idle, &web, None));
        assert!(should_fire(NotificationEvent::Error, &web, None));

        // Instance with no overrides: inherits web defaults.
        let mut inst = Instance::new("t", "/tmp");
        assert!(should_fire(NotificationEvent::Waiting, &web, Some(&inst)));
        assert!(!should_fire(NotificationEvent::Idle, &web, Some(&inst)));
        assert!(should_fire(NotificationEvent::Error, &web, Some(&inst)));

        // Session opts INTO idle despite global default off; this is
        // the "I want to babysit this one long session" case.
        inst.notify_on_idle = Some(true);
        assert!(should_fire(NotificationEvent::Idle, &web, Some(&inst)));

        // Session opts OUT of waiting despite global on; this is the
        // "stop spamming me about this noisy session" case.
        inst.notify_on_waiting = Some(false);
        assert!(!should_fire(NotificationEvent::Waiting, &web, Some(&inst)));

        // Error unaffected: per-event-type overrides don't cross-pollute.
        assert!(should_fire(NotificationEvent::Error, &web, Some(&inst)));
    }

    #[test]
    fn notification_delivery_requires_a_current_live_matching_status() {
        use crate::session::Instance;

        let mut inst = Instance::new("t", "/tmp");
        inst.status = Status::Waiting;
        assert!(notification_matches_live_instance(
            NotificationEvent::Waiting,
            &inst
        ));
        assert!(!notification_matches_live_instance(
            NotificationEvent::Error,
            &inst
        ));

        // Trash deliberately stops the pane.
        inst.trash();
        assert!(!notification_matches_live_instance(
            NotificationEvent::Waiting,
            &inst
        ));
    }
}
