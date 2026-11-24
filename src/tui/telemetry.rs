//! The TUI reports telemetry to the Daemon, which owns consent and the install id and
//! applies the opt-in to everything it counts. Usage reports are fire-and-forget: a failed
//! post drops the report. A consent change stays pending until the Daemon acknowledges it;
//! its outcome is kept for the App, and quitting waits for it.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::telemetry::feature_use::FeatureUse;

/// What happened to the latest consent change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConsentOutcome {
    Saved,
    NotSaved(String),
}

type Post = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// The latest finished request's outcome, tagged with its request number.
static CONSENT_OUTCOME: Mutex<Option<(u64, ConsentOutcome)>> = Mutex::new(None);
static CONSENT_PENDING: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static CONSENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CONSENT_ORDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long quitting waits for an unacknowledged consent change.
const QUIT_WAIT: Duration = Duration::from_secs(5);

fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn post(path: &'static str, body: serde_json::Value) -> Result<(), String> {
    let endpoint = crate::acp::client::locate_daemon()
        .await
        .map_err(|e| e.to_string())?;
    crate::acp::client::HttpClient::new(endpoint)
        .map_err(|e| e.to_string())?
        .mutate(reqwest::Method::POST, path, Some(body))
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

fn report(path: &'static str, body: serde_json::Value) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        if let Err(error) = post(path, body).await {
            tracing::debug!(target: "telemetry", path, "telemetry report failed: {error}");
        }
    });
}

/// Saves an opt-in answer through the Daemon, which also marks the prompt answered. Only
/// the latest of several quick changes is sent, and only its outcome is reported, through
/// [`take_consent_outcome`] and [`finish_pending_consent`].
pub(crate) fn set_consent(enabled: bool) {
    send_consent(Box::pin(post(
        "/api/telemetry/consent",
        serde_json::json!({ "enabled": enabled }),
    )));
}

fn send_consent(request: Post) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let sequence = CONSENT_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    let task = runtime.spawn(async move {
        let _order = CONSENT_ORDER.lock().await;
        if CONSENT_SEQUENCE.load(Ordering::SeqCst) != sequence {
            return;
        }
        let outcome = match request.await {
            Ok(()) => ConsentOutcome::Saved,
            Err(error) => {
                tracing::warn!(target: "telemetry", "telemetry consent not saved: {error}");
                ConsentOutcome::NotSaved(format!("Telemetry setting not saved: {error}"))
            }
        };
        *guard(&CONSENT_OUTCOME) = Some((sequence, outcome));
    });
    // The latest task waits behind every earlier one, so it alone marks completion.
    *guard(&CONSENT_PENDING) = Some(task);
}

/// The outcome of the latest consent change, once, after the Daemon answered. An older
/// request's outcome is dropped: the newer change reports its own.
pub(crate) fn take_consent_outcome() -> Option<ConsentOutcome> {
    let (sequence, outcome) = guard(&CONSENT_OUTCOME).take()?;
    (sequence == CONSENT_SEQUENCE.load(Ordering::SeqCst)).then_some(outcome)
}

/// Whether a consent change is still waiting for the Daemon's answer.
pub(crate) fn consent_in_flight() -> bool {
    guard(&CONSENT_PENDING)
        .as_ref()
        .is_some_and(|task| !task.is_finished())
}

/// Waits for an unacknowledged consent change before the TUI exits. Returns the message
/// to show when it was not saved and the App has not shown that yet.
pub(crate) async fn finish_pending_consent() -> Option<String> {
    finish_pending_consent_within(QUIT_WAIT).await
}

async fn finish_pending_consent_within(wait: Duration) -> Option<String> {
    let task = guard(&CONSENT_PENDING).take()?;
    if tokio::time::timeout(wait, task).await.is_err() {
        // The request may have reached the Daemon, which finishes a started save.
        return Some(
            "Telemetry setting may not have been saved: the Daemon did not answer in time".into(),
        );
    }
    match take_consent_outcome() {
        Some(ConsentOutcome::NotSaved(message)) => Some(message),
        Some(ConsentOutcome::Saved) | None => None,
    }
}

pub(crate) fn report_feature_use(feature: FeatureUse) {
    report(
        "/api/telemetry/feature-use",
        serde_json::json!({ "feature": feature.as_str() }),
    );
}

/// Marks the TUI as used in the Daemon's next snapshot.
pub(crate) fn report_seen() {
    report(
        "/api/telemetry/seen",
        serde_json::json!({ "surface": "tui" }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    /// A request that signals when it is sent and answers when released.
    fn delayed(
        started: oneshot::Sender<()>,
        release: oneshot::Receiver<Result<(), String>>,
    ) -> Post {
        Box::pin(async move {
            started.send(()).ok();
            release.await.unwrap_or_else(|_| Err("dropped".into()))
        })
    }

    fn request() -> (
        Post,
        oneshot::Receiver<()>,
        oneshot::Sender<Result<(), String>>,
    ) {
        let (started, sent) = oneshot::channel();
        let (answer, release) = oneshot::channel();
        (delayed(started, release), sent, answer)
    }

    /// Waits for the latest consent request to finish, failing instead of hanging.
    async fn settled() {
        tokio::time::timeout(Duration::from_secs(5), async {
            while consent_in_flight() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the consent request finishes");
    }

    /// Quitting waits for a delayed consent request: an answer in time is saved silently,
    /// a failure or no answer is reported, and a superseded change reports nothing.
    #[tokio::test]
    #[serial_test::serial]
    async fn quitting_finishes_a_delayed_consent_change_or_reports_it_unsaved() {
        let wait = Duration::from_secs(5);

        let (post, sent, answer) = request();
        send_consent(post);
        sent.await.unwrap();
        assert!(consent_in_flight());
        let quit = tokio::spawn(finish_pending_consent_within(wait));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!quit.is_finished(), "quitting waits for the answer");
        answer.send(Ok(())).unwrap();
        assert_eq!(quit.await.unwrap(), None);
        assert!(!consent_in_flight());
        assert_eq!(take_consent_outcome(), None, "the outcome is consumed once");

        let (post, _sent, answer) = request();
        send_consent(post);
        answer.send(Err("refused".into())).unwrap();
        assert_eq!(
            finish_pending_consent_within(wait).await.as_deref(),
            Some("Telemetry setting not saved: refused")
        );

        let (post, _sent, unanswered) = request();
        send_consent(post);
        assert!(finish_pending_consent_within(Duration::from_millis(50))
            .await
            .is_some_and(|message| message.contains("may not have been saved")));
        drop(unanswered);

        // The first request fails in flight after a newer change was queued.
        let (first, first_sent, first_answer) = request();
        send_consent(first);
        first_sent.await.unwrap();
        let (second, second_sent, second_answer) = request();
        send_consent(second);
        first_answer.send(Err("superseded".into())).unwrap();
        second_sent.await.unwrap();
        assert_eq!(
            take_consent_outcome(),
            None,
            "a superseded failure is dropped"
        );
        second_answer.send(Ok(())).unwrap();
        assert_eq!(finish_pending_consent_within(wait).await, None);

        // A failure that finished unread never answers for a newer pending request.
        let (stale, _stale_sent, stale_answer) = request();
        send_consent(stale);
        stale_answer.send(Err("stale".into())).unwrap();
        settled().await;
        let (newer, newer_sent, newer_answer) = request();
        send_consent(newer);
        newer_sent.await.unwrap();
        assert_eq!(
            take_consent_outcome(),
            None,
            "the stale failure is not applied"
        );
        newer_answer.send(Ok(())).unwrap();
        settled().await;
        assert_eq!(take_consent_outcome(), Some(ConsentOutcome::Saved));
    }
}
