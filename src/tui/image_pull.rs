//! Reads, starts and cancels the sandbox image pull for the home screen: through the Daemon when
//! one is running, else through this process's own pull.

use std::time::{Duration, Instant};

use crate::acp::client::http::{SandboxImageAvailability, SandboxImageStatus};
use crate::acp::client::{DiscoveryError, HttpClient, HttpError, ManagerError};
use crate::containers::image_pull::PullState;
use crate::containers::image_update::ImageUpdate;
use crate::containers::pull_progress::ProgressSnapshot;
use crate::containers::ContainerRuntime;
use crate::daemon::DaemonEndpoint;
use crate::session::ContainerRuntimeName;
use crate::tui::home::update_bar::PullLine;

/// How long a local Cancel waits for the killed pull to end, as the Daemon's Cancel does.
const CANCEL_BACKSTOP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullRequest {
    Poll,
    Start,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullReport {
    State(PullState),
    /// The Daemon won't pull; `command` is what to run instead.
    Gated {
        command: String,
    },
    /// No answer: the Daemon is unreachable or failed the request.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullAnswer {
    pub(crate) report: PullReport,
    /// Only a Daemon's poll says: with a Daemon, its offer replaces this TUI's own registry
    /// check, so both clients offer the same update.
    pub(crate) offer: Option<ImageOffer>,
}

impl From<PullReport> for PullAnswer {
    fn from(report: PullReport) -> Self {
        Self {
            report,
            offer: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageOffer {
    Update(ImageUpdate),
    /// Current, dismissed, unchecked, or no sandbox in use.
    NoUpdate,
}

impl ImageOffer {
    fn from_status(status: &SandboxImageStatus) -> Self {
        match &status.state {
            SandboxImageAvailability::Available { remote_digest }
                if status.sandbox_in_use && !status.dismissed =>
            {
                Self::Update(ImageUpdate {
                    image: status.image.clone(),
                    remote_digest: remote_digest.clone(),
                })
            }
            _ => Self::NoUpdate,
        }
    }
}

pub(crate) enum PullSource {
    Daemon(HttpClient),
    /// No Daemon is running, so this process owns the pull.
    Local,
    /// A Daemon exists but can't be used. Never pull locally then, or two pulls would run.
    Unavailable,
}

impl PullSource {
    pub(crate) async fn locate() -> Self {
        Self::from_lookup(crate::acp::client::locate_daemon().await)
    }

    fn from_lookup(lookup: Result<DaemonEndpoint, ManagerError>) -> Self {
        match lookup {
            Ok(endpoint) => HttpClient::new(endpoint).map_or(Self::Unavailable, Self::Daemon),
            Err(ManagerError::NoDaemonRunning(DiscoveryError::NoLocalDaemon)) => Self::Local,
            Err(_) => Self::Unavailable,
        }
    }

    /// `runtime` and `image` name the local pull; the Daemon pulls its own configured image.
    pub(crate) async fn run(
        &self,
        runtime: ContainerRuntimeName,
        image: &str,
        request: PullRequest,
    ) -> PullAnswer {
        match self {
            Self::Daemon(client) => daemon(client, request).await,
            Self::Local => local(runtime, image.to_string(), request).await.into(),
            Self::Unavailable => PullReport::Unavailable.into(),
        }
    }
}

async fn daemon(client: &HttpClient, request: PullRequest) -> PullAnswer {
    let answer = match request {
        PullRequest::Poll => client
            .sandbox_image_status(false)
            .await
            .map(|status| (Some(ImageOffer::from_status(&status)), status.pull)),
        PullRequest::Start => client.sandbox_image_pull().await.map(|pull| (None, pull)),
        PullRequest::Cancel => client.sandbox_image_cancel().await.map(|pull| (None, pull)),
    };
    match answer {
        Ok((offer, pull)) => PullAnswer {
            report: PullReport::State(pull),
            offer,
        },
        Err(error) if is_refusal(&error) => match client.sandbox_image_status(false).await {
            Ok(status) => PullReport::Gated {
                command: status.pull_command,
            }
            .into(),
            Err(_) => PullReport::Unavailable.into(),
        },
        Err(_) => PullReport::Unavailable.into(),
    }
}

/// The Daemon refuses a pull or cancel when it is read-only, a CityHall client, or has
/// dashboard pulls turned off.
fn is_refusal(error: &HttpError) -> bool {
    matches!(error, HttpError::ReadOnly)
        || matches!(error, HttpError::Server { status, .. } if *status == reqwest::StatusCode::FORBIDDEN)
}

async fn local(runtime: ContainerRuntimeName, image: String, request: PullRequest) -> PullReport {
    tokio::task::spawn_blocking(move || {
        let runtime = ContainerRuntime::from(runtime);
        match request {
            PullRequest::Poll => runtime.pull_state(&image),
            PullRequest::Start => {
                // A flight runs only while it has a waiter, so this pull keeps one to its end.
                let waiter = runtime.join_pull(&image);
                std::thread::spawn(move || {
                    let _ = waiter.wait(&tokio_util::sync::CancellationToken::new());
                });
                runtime.pull_state(&image)
            }
            PullRequest::Cancel => runtime.cancel_pull(&image, CANCEL_BACKSTOP),
        }
    })
    .await
    .map_or(PullReport::Unavailable, PullReport::State)
}

/// How long the first Ctrl+E waits for the second.
pub(crate) const CANCEL_CONFIRM_TTL: Duration = Duration::from_secs(3);

/// How often the TUI reads the pull's state: often while it runs, so the meter moves, and
/// otherwise slowly, to notice a pull another client started.
pub(crate) fn poll_interval(running: bool) -> Duration {
    Duration::from_secs(if running { 1 } else { 5 })
}

/// The home screen's view of the sandbox image pull: what its line shows and what its keys do.
/// It shows an outcome only for a pull it watched, one it started or saw running, so an
/// outcome already acknowledged elsewhere or before a restart doesn't return.
#[derive(Debug, Default)]
pub(crate) struct PullWatch {
    line: Option<PullLine>,
    /// Ctrl+X on a running pull hides its line until the pull ends.
    hidden: bool,
    watched: bool,
    cancel_armed: Option<Instant>,
}

/// What [`PullWatch::apply`] changed, beyond the line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Unchanged,
    Changed,
    /// The pull finished: the image the update bar offered is now local.
    Updated,
    /// The request got no answer; the text says what didn't happen.
    Unanswered(&'static str),
}

/// What a Ctrl+E did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CancelKey {
    /// No pull is running.
    Ignored,
    /// The first press: ask for a second within [`CANCEL_CONFIRM_TTL`].
    Confirm,
    /// The second press in time: cancel.
    Cancel,
}

impl PullWatch {
    /// The line the update bar shows, if any. A hidden running pull shows none.
    pub(crate) fn line(&self) -> Option<&PullLine> {
        self.line
            .as_ref()
            .filter(|_| !(self.hidden && self.running()))
    }

    /// Whether the update bar holds a pull, shown or hidden, running or waiting to be
    /// acknowledged; the sandbox image offer stays back meanwhile.
    pub(crate) fn holds_line(&self) -> bool {
        self.line.is_some()
    }

    pub(crate) fn running(&self) -> bool {
        matches!(self.line, Some(PullLine::Running(_)))
    }

    /// The progress a hidden running pull shows at the end of the key line.
    pub(crate) fn hidden_progress(&self) -> Option<&ProgressSnapshot> {
        match &self.line {
            Some(PullLine::Running(snapshot)) if self.hidden => Some(snapshot),
            _ => None,
        }
    }

    /// A pull this TUI asked for runs from now, before the first answer arrives.
    pub(crate) fn start(&mut self) {
        self.set_line(Some(PullLine::Running(ProgressSnapshot::default())));
        self.watched = true;
    }

    pub(crate) fn apply(&mut self, request: PullRequest, report: PullReport) -> Applied {
        let before = self.line.clone();
        let mut applied = Applied::Changed;
        match report {
            PullReport::State(PullState::Running(snapshot)) => {
                self.watched = true;
                self.set_line(Some(PullLine::Running(snapshot)));
            }
            PullReport::State(PullState::Idle) => {
                // A pull this TUI just started may not have registered yet.
                if self.running() && request != PullRequest::Start {
                    self.end(None);
                }
            }
            PullReport::State(_) if !self.watched => {}
            PullReport::State(PullState::Done) => {
                self.end(Some(PullLine::Done));
                applied = Applied::Updated;
            }
            PullReport::State(PullState::Cancelled) => self.end(Some(PullLine::Cancelled)),
            PullReport::State(PullState::Failed { reason, .. }) => {
                self.end(Some(PullLine::Failed(reason)))
            }
            PullReport::Gated { command } => self.end(Some(PullLine::Gated { command })),
            PullReport::Unavailable => match request {
                PullRequest::Poll => {}
                PullRequest::Start => {
                    self.end(None);
                    return Applied::Unanswered(
                        "Couldn't start the sandbox image download: the Daemon didn't answer.",
                    );
                }
                PullRequest::Cancel => {
                    return Applied::Unanswered(
                        "Couldn't cancel the sandbox image download: the Daemon didn't answer.",
                    );
                }
            },
        }
        if applied == Applied::Changed && self.line == before {
            return Applied::Unchanged;
        }
        applied
    }

    /// Ctrl+X on the drawn pull line, if it is still `drawn`: hides a running pull and
    /// acknowledges an outcome. Returns whether it was still current.
    pub(crate) fn dismiss(&mut self, drawn: &PullLine) -> bool {
        if self.line.as_ref() != Some(drawn) {
            return false;
        }
        if self.running() {
            self.hidden = true;
        } else {
            self.set_line(None);
        }
        true
    }

    /// Whether the Update key on the drawn line retries the pull.
    pub(crate) fn retries(&self, drawn: &PullLine) -> bool {
        self.line.as_ref() == Some(drawn)
            && matches!(drawn, PullLine::Failed(_) | PullLine::Cancelled)
    }

    pub(crate) fn cancel_key(&mut self, now: Instant) -> CancelKey {
        if !self.running() {
            return CancelKey::Ignored;
        }
        match self.cancel_armed.take() {
            Some(at) if now.saturating_duration_since(at) <= CANCEL_CONFIRM_TTL => {
                CancelKey::Cancel
            }
            _ => {
                self.cancel_armed = Some(now);
                CancelKey::Confirm
            }
        }
    }

    /// Any key but Ctrl+E ends a cancel prompt, and a Done line clears so the key acts as
    /// usual. Returns whether the line changed.
    pub(crate) fn other_key(&mut self) -> bool {
        self.cancel_armed = None;
        if self.line == Some(PullLine::Done) {
            self.set_line(None);
            return true;
        }
        false
    }

    fn end(&mut self, outcome: Option<PullLine>) {
        self.watched = false;
        self.set_line(outcome);
    }

    fn set_line(&mut self, line: Option<PullLine>) {
        if !matches!(line, Some(PullLine::Running(_))) {
            self.hidden = false;
            self.cancel_armed = None;
        }
        self.line = line;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::image_pull::{self, PullKey};
    use crate::server::test_support;
    use std::time::Instant;

    const IMAGE: &str = "aoe-test/tui-pull-source:latest";

    fn running(done: u32) -> PullReport {
        PullReport::State(PullState::Running(ProgressSnapshot {
            layers_done: Some(done),
            layers_known: Some(7),
            ..ProgressSnapshot::default()
        }))
    }

    fn failed() -> PullReport {
        PullReport::State(PullState::Failed {
            reason: image_pull::PullFailureReason::Network,
            message: String::new(),
            error: String::new(),
        })
    }

    /// An outcome shows only for a pull this TUI watched; a pull nobody here watched, or one
    /// whose outcome this TUI already showed, leaves the bar alone.
    #[test]
    fn a_pull_shows_its_outcome_only_once_watched() {
        use PullRequest::{Cancel, Poll, Start};
        let done = || PullReport::State(PullState::Done);
        let cancelled = || PullReport::State(PullState::Cancelled);
        let idle = || PullReport::State(PullState::Idle);
        let gated = || PullReport::Gated {
            command: "docker pull img".into(),
        };
        let shown = |line: PullLine| Some(line);
        // (case, started here, reports in order, line after, applied by the last report)
        type Case = (
            &'static str,
            bool,
            Vec<(PullRequest, PullReport)>,
            Option<PullLine>,
            Applied,
        );
        let cases: Vec<Case> = vec![
            (
                "unwatched done",
                false,
                vec![(Poll, done())],
                None,
                Applied::Unchanged,
            ),
            (
                "unwatched failure",
                false,
                vec![(Poll, failed())],
                None,
                Applied::Unchanged,
            ),
            (
                "seen running, then done",
                false,
                vec![(Poll, running(3)), (Poll, done())],
                shown(PullLine::Done),
                Applied::Updated,
            ),
            (
                "started, then done",
                true,
                vec![(Poll, done())],
                shown(PullLine::Done),
                Applied::Updated,
            ),
            (
                "started, then failed",
                true,
                vec![(Poll, failed())],
                shown(PullLine::Failed(image_pull::PullFailureReason::Network)),
                Applied::Changed,
            ),
            (
                "cancelled",
                true,
                vec![(Cancel, cancelled())],
                shown(PullLine::Cancelled),
                Applied::Changed,
            ),
            (
                "an outcome shows once",
                true,
                vec![(Poll, done()), (Poll, done())],
                shown(PullLine::Done),
                Applied::Unchanged,
            ),
            (
                "refused",
                true,
                vec![(Start, gated())],
                shown(PullLine::Gated {
                    command: "docker pull img".into(),
                }),
                Applied::Changed,
            ),
            (
                "a start not registered yet",
                true,
                vec![(Start, idle())],
                shown(PullLine::Running(ProgressSnapshot::default())),
                Applied::Unchanged,
            ),
            (
                "a pull that vanished",
                false,
                vec![(Poll, running(3)), (Poll, idle())],
                None,
                Applied::Changed,
            ),
            (
                "no answer to a poll",
                true,
                vec![(Poll, PullReport::Unavailable)],
                shown(PullLine::Running(ProgressSnapshot::default())),
                Applied::Unchanged,
            ),
            (
                "no answer to a start",
                true,
                vec![(Start, PullReport::Unavailable)],
                None,
                Applied::Unanswered(
                    "Couldn't start the sandbox image download: the Daemon didn't answer.",
                ),
            ),
            (
                "no answer to a cancel",
                true,
                vec![(Cancel, PullReport::Unavailable)],
                shown(PullLine::Running(ProgressSnapshot::default())),
                Applied::Unanswered(
                    "Couldn't cancel the sandbox image download: the Daemon didn't answer.",
                ),
            ),
        ];
        for (case, started, reports, line, applied) in cases {
            let mut watch = PullWatch::default();
            if started {
                watch.start();
            }
            let mut last = Applied::Unchanged;
            for (request, report) in reports {
                last = watch.apply(request, report);
            }
            assert_eq!(watch.line(), line.as_ref(), "{case}");
            assert_eq!(last, applied, "{case}");
        }
    }

    /// Ctrl+X hides a running pull, whose progress stays for the key line, until it ends;
    /// on an outcome it acknowledges. A line no longer current is left alone.
    #[test]
    fn dismiss_hides_a_running_pull_and_acknowledges_an_outcome() {
        let mut watch = PullWatch::default();
        watch.apply(PullRequest::Poll, running(3));
        let drawn = watch.line().cloned().unwrap();
        watch.apply(PullRequest::Poll, running(4));
        assert!(!watch.dismiss(&drawn), "a line since redrawn was dismissed");
        let drawn = watch.line().cloned().unwrap();
        assert!(watch.dismiss(&drawn));
        assert_eq!(watch.line(), None);
        assert!(watch.holds_line(), "a hidden pull let the image offer back");
        assert_eq!(watch.hidden_progress().and_then(|p| p.layers_done), Some(4));
        watch.apply(PullRequest::Poll, running(5));
        assert_eq!(watch.line(), None, "a new reading unhid the pull");

        watch.apply(PullRequest::Poll, failed());
        let outcome = watch
            .line()
            .cloned()
            .expect("the outcome shows though the pull was hidden");
        assert_eq!(watch.hidden_progress(), None);
        assert!(watch.retries(&outcome));
        assert!(watch.dismiss(&outcome));
        assert_eq!(watch.line(), None);
        assert!(!watch.holds_line());
    }

    /// The first Ctrl+E asks; a second within the prompt's window cancels, one after it asks
    /// again, and any other key ends the prompt. Done clears on any key.
    #[test]
    fn ctrl_e_cancels_only_when_pressed_twice_in_time() {
        let at = Instant::now();
        let mut watch = PullWatch::default();
        assert_eq!(
            watch.cancel_key(at),
            CancelKey::Ignored,
            "nothing to cancel"
        );
        watch.start();
        for (second, expected) in [
            (at + CANCEL_CONFIRM_TTL, CancelKey::Cancel),
            (
                at + CANCEL_CONFIRM_TTL + Duration::from_millis(1),
                CancelKey::Confirm,
            ),
        ] {
            assert_eq!(watch.cancel_key(at), CancelKey::Confirm);
            assert_eq!(watch.cancel_key(second), expected, "{:?}", second - at);
            watch.other_key();
        }
        assert_eq!(watch.cancel_key(at), CancelKey::Confirm);
        assert!(!watch.other_key(), "a running pull's line changed on a key");
        assert_eq!(
            watch.cancel_key(at),
            CancelKey::Confirm,
            "another key left the prompt"
        );

        watch.apply(PullRequest::Poll, PullReport::State(PullState::Done));
        assert!(watch.other_key());
        assert_eq!(watch.line(), None);
    }

    /// A Daemon offers an update only when one is available, a sandbox is in use and nobody
    /// dismissed it.
    #[test]
    fn a_daemon_offers_only_an_available_update_in_use_and_not_dismissed() {
        let update = ImageUpdate {
            image: IMAGE.into(),
            remote_digest: "sha256:new".into(),
        };
        let available = SandboxImageAvailability::Available {
            remote_digest: "sha256:new".into(),
        };
        let unknown = SandboxImageAvailability::Unknown {
            reason: "checks_off".into(),
        };
        let failed = SandboxImageAvailability::CheckFailed {
            error: "registry_unreachable".into(),
        };
        // (state, sandbox in use, dismissed, offer)
        for (state, in_use, dismissed, offer) in [
            (&available, true, false, ImageOffer::Update(update.clone())),
            (&available, true, true, ImageOffer::NoUpdate),
            (&available, false, false, ImageOffer::NoUpdate),
            (
                &SandboxImageAvailability::Current,
                true,
                false,
                ImageOffer::NoUpdate,
            ),
            (&unknown, true, false, ImageOffer::NoUpdate),
            (&failed, true, false, ImageOffer::NoUpdate),
        ] {
            let status = SandboxImageStatus {
                image: IMAGE.into(),
                state: state.clone(),
                dismissed,
                checks_enabled: true,
                sandbox_in_use: in_use,
                pull: PullState::Idle,
                pull_blocked: None,
                pull_command: String::new(),
            };
            assert_eq!(
                ImageOffer::from_status(&status),
                offer,
                "{state:?} in_use={in_use} dismissed={dismissed}"
            );
        }
    }

    fn key() -> PullKey {
        PullKey {
            runtime: ContainerRuntime::from(ContainerRuntimeName::default()).display_name(),
            image: IMAGE.into(),
        }
    }

    /// A stand-in pull that runs until cancelled, or ends itself at a deadline so a broken
    /// cancel fails the test instead of hanging it.
    fn stand_in() -> image_pull::Waiter {
        let deadline = Instant::now() + Duration::from_secs(10);
        image_pull::join_with(key(), move |pull| {
            while !pull.kill.is_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(crate::containers::error::DockerError::Cancelled(
                "pull".into(),
            ))
        })
    }

    #[test]
    fn only_a_missing_local_daemon_pulls_locally() {
        let endpoint = DaemonEndpoint::new(
            "http://127.0.0.1:1".into(),
            None,
            crate::daemon::Source::Env,
        );
        assert!(matches!(
            PullSource::from_lookup(Ok(endpoint)),
            PullSource::Daemon(_)
        ));
        assert!(matches!(
            PullSource::from_lookup(Err(ManagerError::NoDaemonRunning(
                DiscoveryError::NoLocalDaemon
            ))),
            PullSource::Local
        ));
        for unusable in [
            ManagerError::NoDaemonRunning(DiscoveryError::Unreachable),
            ManagerError::NoDaemonRunning(DiscoveryError::LocalPath),
            ManagerError::EnvOverrideUnreachable,
            ManagerError::EnvOverrideUnauthorized,
        ] {
            let label = unusable.to_string();
            assert!(
                matches!(
                    PullSource::from_lookup(Err(unusable)),
                    PullSource::Unavailable
                ),
                "{label}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_local_start_keeps_a_waiter_and_cancel_ends_the_pull() {
        let runtime = ContainerRuntimeName::default();
        let source = PullSource::Local;
        assert_eq!(
            source.run(runtime, IMAGE, PullRequest::Poll).await,
            PullAnswer::from(PullReport::State(PullState::Idle)),
            "no offer: without a Daemon, the startup check offers the update"
        );
        let flight = stand_in();
        assert!(matches!(
            source.run(runtime, IMAGE, PullRequest::Start).await.report,
            PullReport::State(PullState::Running(_))
        ));
        assert_eq!(
            image_pull::waiters_for_test(&key()),
            2,
            "the stand-in and Start's"
        );
        assert_eq!(
            source.run(runtime, IMAGE, PullRequest::Cancel).await.report,
            PullReport::State(PullState::Cancelled)
        );
        assert!(flight
            .wait(&tokio_util::sync::CancellationToken::new())
            .is_err());
    }

    /// A Daemon that won't pull reports the command to run instead, and nothing pulls here.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_daemon_pull_is_gated_and_never_pulls_locally() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|c| {
            c.updates.update_check_mode = crate::session::config::UpdateCheckMode::Off;
            c.sandbox.allow_image_pull = true;
            c.sandbox.default_image = IMAGE.into();
        })
        .unwrap();
        let state =
            test_support::build_test_app_state_configured(Vec::new(), |s| s.read_only = true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = test_support::build_router_for_test(state.clone());
        let shutdown = state.shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        let endpoint = DaemonEndpoint::new(base, None, crate::daemon::Source::Env);
        let source = PullSource::Daemon(HttpClient::new(endpoint).unwrap());
        let runtime = ContainerRuntimeName::default();

        assert_eq!(
            source.run(runtime, IMAGE, PullRequest::Poll).await,
            PullAnswer {
                report: PullReport::State(PullState::Idle),
                offer: Some(ImageOffer::NoUpdate),
            },
            "checks are off, so the Daemon offers nothing"
        );
        for request in [PullRequest::Start, PullRequest::Cancel] {
            let PullReport::Gated { command } = source.run(runtime, IMAGE, request).await.report
            else {
                panic!("{request:?} should be gated");
            };
            assert!(command.contains(IMAGE), "{command}");
        }
        assert_eq!(image_pull::waiters_for_test(&key()), 0);
        assert_eq!(
            ContainerRuntime::from(runtime).pull_state(IMAGE),
            PullState::Idle
        );
        state.shutdown.cancel();
        server.await.unwrap();
    }
}
