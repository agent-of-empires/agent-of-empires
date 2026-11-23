//! Reads, starts and cancels the sandbox image pull for the home screen, through the Daemon.

use std::time::{Duration, Instant};

use crate::acp::client::http::{SandboxImageAvailability, SandboxImageStatus};
use crate::acp::client::{HttpClient, HttpError, ManagerError};
use crate::containers::image_pull::PullState;
use crate::containers::image_update::ImageUpdate;
use crate::containers::pull_progress::ProgressSnapshot;
use crate::daemon::DaemonEndpoint;
use crate::tui::home::update_bar::PullLine;

const START_UNANSWERED: &str =
    "Couldn't start the sandbox image download: the Daemon didn't answer.";
const CANCEL_GATED: &str =
    "Couldn't cancel the sandbox image download: image pulls are off on the Daemon.";

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
    /// The Daemon no longer has this image configured.
    UnknownImage,
    /// No answer: the Daemon is unreachable or failed the request.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullAnswer {
    pub(crate) report: PullReport,
    /// Only a poll carries the Daemon's offer, and the image it reported on.
    pub(crate) offer: Option<ImageOffer>,
    pub(crate) image: Option<String>,
}

impl From<PullReport> for PullAnswer {
    fn from(report: PullReport) -> Self {
        Self {
            report,
            offer: None,
            image: None,
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

/// Without a usable Daemon nothing pulls: the home screen shows the offline gate then.
pub(crate) enum PullSource {
    Daemon(HttpClient),
    Unavailable,
}

impl PullSource {
    pub(crate) async fn locate() -> Self {
        Self::from_lookup(crate::acp::client::locate_daemon().await)
    }

    fn from_lookup(lookup: Result<DaemonEndpoint, ManagerError>) -> Self {
        lookup
            .ok()
            .and_then(|endpoint| HttpClient::new(endpoint).ok())
            .map_or(Self::Unavailable, Self::Daemon)
    }

    /// `image` is the pull's image; empty means the Daemon's configured default.
    pub(crate) async fn run(&self, image: &str, request: PullRequest) -> PullAnswer {
        match self {
            Self::Daemon(client) => daemon(client, image, request).await,
            Self::Unavailable => PullReport::Unavailable.into(),
        }
    }
}

async fn daemon(client: &HttpClient, image: &str, request: PullRequest) -> PullAnswer {
    let answer = match request {
        PullRequest::Poll => client
            .sandbox_image_status(false, image)
            .await
            .map(|status| {
                let offer = ImageOffer::from_status(&status);
                (Some((offer, status.image)), status.pull)
            }),
        PullRequest::Start => client
            .sandbox_image_pull(image)
            .await
            .map(|pull| (None, pull)),
        PullRequest::Cancel => client
            .sandbox_image_cancel(image)
            .await
            .map(|pull| (None, pull)),
    };
    match answer {
        Ok((polled, pull)) => {
            let (offer, image) = polled.unzip();
            PullAnswer {
                report: PullReport::State(pull),
                offer,
                image,
            }
        }
        Err(error) if is_unknown_image(&error) => PullReport::UnknownImage.into(),
        Err(error) if is_refusal(&error) => match client.sandbox_image_status(false, image).await {
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

/// The client classifies by the `AoE-Error-Code` header and keeps only that code as `body`.
fn is_unknown_image(error: &HttpError) -> bool {
    matches!(error, HttpError::Server { status, body }
        if *status == reqwest::StatusCode::BAD_REQUEST
            && body == crate::daemon::ApiErrorCode::UnknownImage.as_str())
}

/// How long Ctrl+E's prompt waits for its confirming key.
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
    /// The Daemon reported this pull running since it was started here.
    seen_running: bool,
    /// The Start got no reply, so whether it registered is unknown until a poll says.
    start_unanswered: bool,
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
    /// The Start got no answer, but the Daemon may have registered it: poll now.
    Recheck,
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
        self.seen_running = false;
        self.start_unanswered = false;
    }

    pub(crate) fn apply(&mut self, request: PullRequest, report: PullReport) -> Applied {
        let before = self.line.clone();
        let mut applied = Applied::Changed;
        match report {
            PullReport::State(PullState::Running(snapshot)) => {
                self.watched = true;
                self.seen_running = true;
                self.start_unanswered = false;
                self.set_line(Some(PullLine::Running(snapshot)));
            }
            PullReport::State(PullState::Idle) => {
                // A pull this TUI just started may not have registered yet.
                if self.running() && request != PullRequest::Start {
                    let unanswered = self.start_unanswered;
                    self.end(None);
                    if unanswered {
                        return Applied::Unanswered(START_UNANSWERED);
                    }
                }
            }
            PullReport::State(_) if !self.watched => {}
            // Without a reply to the Start, an outcome is this pull's only once a poll saw it run;
            // before that it may be an earlier pull's.
            PullReport::State(_) if self.start_unanswered && !self.seen_running => {
                self.end(None);
                return Applied::Unanswered(START_UNANSWERED);
            }
            PullReport::State(PullState::Done) => {
                self.end(Some(PullLine::Done));
                applied = Applied::Updated;
            }
            PullReport::State(PullState::Cancelled) => self.end(Some(PullLine::Cancelled)),
            PullReport::State(PullState::Failed { reason, .. }) => {
                self.end(Some(PullLine::Failed(reason)))
            }
            // A refused cancel leaves the download running, and hidden if it was.
            PullReport::Gated { .. } if request == PullRequest::Cancel => {
                return Applied::Unanswered(CANCEL_GATED);
            }
            PullReport::Gated { command } => self.end(Some(PullLine::Gated { command })),
            PullReport::UnknownImage => {
                if !self.running() {
                    return Applied::Unchanged;
                }
                self.end(None);
                return Applied::Unanswered(if request == PullRequest::Poll {
                    "The Daemon no longer has this sandbox image configured, so its download isn't shown."
                } else {
                    "The Daemon no longer has this sandbox image configured, so nothing changed."
                });
            }
            PullReport::Unavailable => match request {
                PullRequest::Poll => {}
                PullRequest::Start => {
                    self.start_unanswered = true;
                    return Applied::Recheck;
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

    /// Ctrl+E on a running pull opens the cancel prompt, or restarts its time; it never cancels,
    /// so a held key's repeats can't confirm. Returns whether a pull is running.
    pub(crate) fn arm_cancel(&mut self, now: Instant) -> bool {
        if !self.running() {
            return false;
        }
        self.cancel_armed = Some(now);
        true
    }

    /// The prompt's confirming key: whether it cancels now. Ends the prompt either way.
    pub(crate) fn confirm_cancel(&mut self, now: Instant) -> bool {
        self.cancel_armed.take().is_some_and(|at| {
            self.running() && now.saturating_duration_since(at) <= CANCEL_CONFIRM_TTL
        })
    }

    /// Any key but Ctrl+E or the confirming key ends a cancel prompt, and clears a Done line the
    /// last frame `done_drawn`, so the key acts as usual. Returns whether the line changed.
    pub(crate) fn other_key(&mut self, done_drawn: bool) -> bool {
        self.cancel_armed = None;
        if done_drawn && self.line == Some(PullLine::Done) {
            self.set_line(None);
            return true;
        }
        false
    }

    fn end(&mut self, outcome: Option<PullLine>) {
        self.watched = false;
        self.seen_running = false;
        self.start_unanswered = false;
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
    use crate::containers::image_pull;
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
            // The Daemon may have registered a Start it didn't answer, so the watch stays.
            (
                "no answer to a start",
                true,
                vec![(Start, PullReport::Unavailable)],
                shown(PullLine::Running(ProgressSnapshot::default())),
                Applied::Recheck,
            ),
            (
                "no answer to a start, then it runs and finishes",
                true,
                vec![
                    (Start, PullReport::Unavailable),
                    (Poll, running(3)),
                    (Poll, done()),
                ],
                shown(PullLine::Done),
                Applied::Updated,
            ),
            (
                "no answer to a start, and no pull",
                true,
                vec![(Start, PullReport::Unavailable), (Poll, idle())],
                None,
                Applied::Unanswered(START_UNANSWERED),
            ),
            (
                "no answer to a start, and an earlier pull's outcome",
                true,
                vec![(Start, PullReport::Unavailable), (Poll, done())],
                None,
                Applied::Unanswered(START_UNANSWERED),
            ),
            (
                "an image no longer configured",
                true,
                vec![(Start, PullReport::UnknownImage)],
                None,
                Applied::Unanswered(
                    "The Daemon no longer has this sandbox image configured, so nothing changed.",
                ),
            ),
            (
                "a running pull's image no longer configured",
                true,
                vec![(Poll, running(2)), (Poll, PullReport::UnknownImage)],
                None,
                Applied::Unanswered(
                    "The Daemon no longer has this sandbox image configured, so its download isn't shown.",
                ),
            ),
            (
                "a refused cancel",
                true,
                vec![(Poll, running(2)), (Cancel, gated())],
                shown(PullLine::Running(ProgressSnapshot {
                    layers_done: Some(2),
                    layers_known: Some(7),
                    ..ProgressSnapshot::default()
                })),
                Applied::Unanswered(CANCEL_GATED),
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
        let gated = PullReport::Gated {
            command: "docker pull img".into(),
        };
        assert_eq!(
            watch.apply(PullRequest::Cancel, gated),
            Applied::Unanswered(CANCEL_GATED)
        );
        assert_eq!(watch.line(), None, "a refused cancel unhid the pull");
        assert!(watch.running(), "a refused cancel ended the watch");

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
    /// Ctrl+E only opens the prompt, so repeating it never cancels; the confirming key cancels
    /// within the prompt's time, and any other key ends the prompt.
    #[test]
    fn ctrl_e_asks_and_only_the_confirming_key_cancels() {
        let at = Instant::now();
        let mut watch = PullWatch::default();
        assert!(!watch.arm_cancel(at), "nothing to cancel");
        assert!(!watch.confirm_cancel(at), "no prompt to confirm");
        watch.start();
        for (confirm, expected) in [
            (at + CANCEL_CONFIRM_TTL, true),
            (at + CANCEL_CONFIRM_TTL + Duration::from_millis(1), false),
        ] {
            for repeat in 0..5 {
                assert!(watch.arm_cancel(at), "Ctrl+E #{repeat} didn't ask");
            }
            assert_eq!(
                watch.confirm_cancel(confirm),
                expected,
                "{:?}",
                confirm - at
            );
            assert!(
                !watch.confirm_cancel(confirm),
                "the prompt outlived its answer"
            );
        }
        watch.arm_cancel(at);
        assert!(
            !watch.other_key(true),
            "a running pull's line changed on a key"
        );
        assert!(!watch.confirm_cancel(at), "another key left the prompt");

        watch.apply(PullRequest::Poll, PullReport::State(PullState::Done));
        assert!(
            !watch.other_key(false),
            "a key cleared a Done that wasn't drawn"
        );
        assert!(watch.other_key(true));
        assert_eq!(watch.line(), None);
    }

    #[test]
    fn only_an_unknown_image_refusal_is_one() {
        let server = |status, body: &str| HttpError::Server {
            status,
            body: body.into(),
        };
        let bad = reqwest::StatusCode::BAD_REQUEST;
        for (error, unknown) in [
            (server(bad, "unknown_image"), true),
            (server(bad, "invalid_body"), false),
            // The client never passes the JSON body through, so it doesn't count.
            (
                server(bad, r#"{"error":"unknown_image","message":"x"}"#),
                false,
            ),
            (server(bad, ""), false),
            (
                server(reqwest::StatusCode::FORBIDDEN, "unknown_image"),
                false,
            ),
            (HttpError::Transport, false),
        ] {
            assert_eq!(is_unknown_image(&error), unknown, "{error:?}");
        }
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

    #[test]
    fn only_a_usable_daemon_is_a_source() {
        use crate::acp::client::DiscoveryError;
        let endpoint = DaemonEndpoint::new(
            "http://127.0.0.1:1".into(),
            None,
            crate::daemon::Source::Env,
        );
        assert!(matches!(
            PullSource::from_lookup(Ok(endpoint)),
            PullSource::Daemon(_)
        ));
        // No Daemon pulls nothing here: the home screen is the offline gate then.
        for unusable in [
            ManagerError::NoDaemonRunning(DiscoveryError::NoLocalDaemon),
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

    /// A Daemon that won't pull reports the command to run instead.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_daemon_pull_is_gated_and_never_pulls_locally() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::update_config(|c| {
            c.updates.update_check_mode = crate::session::config::UpdateCheckMode::Off;
            c.sandbox.allow_image_pull = true;
            c.sandbox.default_image = Some(IMAGE.into());
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

        assert_eq!(
            source.run(IMAGE, PullRequest::Poll).await,
            PullAnswer {
                report: PullReport::State(PullState::Idle),
                offer: Some(ImageOffer::NoUpdate),
                image: Some(IMAGE.into()),
            },
            "checks are off, so the Daemon offers nothing"
        );
        for request in [PullRequest::Start, PullRequest::Cancel] {
            let PullReport::Gated { command } = source.run(IMAGE, request).await.report else {
                panic!("{request:?} should be gated");
            };
            assert!(command.contains(IMAGE), "{command}");
        }
        state.shutdown.cancel();
        server.await.unwrap();
    }
}
