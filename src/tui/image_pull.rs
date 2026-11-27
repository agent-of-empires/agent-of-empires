//! Reads, starts and cancels the sandbox image pull for the home screen, through the Daemon.

use std::time::{Duration, Instant};

use crate::acp::client::http::{SandboxImageAvailability, SandboxImageStatus};
use crate::acp::client::{HttpClient, HttpError, ManagerError};
use crate::containers::image_pull::PullState;
use crate::containers::image_update::ImageUpdate;
use crate::containers::pull_progress::ProgressSnapshot;
use crate::daemon::DaemonEndpoint;
use crate::tui::home::update_bar::{DrawnPull, PullLine};

const START_UNANSWERED: &str =
    "Couldn't start the sandbox image download: the Daemon didn't answer.";
const CANCEL_GATED: &str =
    "Couldn't cancel the sandbox image download: image pulls are off on the Daemon.";
pub(crate) const CANCEL_REPLACED: &str =
    "That download already ended and another one is running, so nothing was cancelled.";

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
    /// The cancel named a pull that has ended; the one running now replaced it.
    Replaced,
    /// No answer: the Daemon is unreachable or failed the request.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullAnswer {
    pub(crate) report: PullReport,
    /// Only a poll carries the Daemon's offer, and the image it reported on.
    pub(crate) offer: Option<ImageOffer>,
    pub(crate) image: Option<String>,
    /// The running pull's id, for a cancel to name.
    pub(crate) flight: Option<u64>,
}

impl From<PullReport> for PullAnswer {
    fn from(report: PullReport) -> Self {
        Self {
            report,
            offer: None,
            image: None,
            flight: None,
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
    /// `flight` is a cancel's: the pull it was asked for.
    pub(crate) async fn run(
        &self,
        image: &str,
        request: PullRequest,
        flight: Option<u64>,
    ) -> PullAnswer {
        match self {
            Self::Daemon(client) => daemon(client, image, request, flight).await,
            Self::Unavailable => PullReport::Unavailable.into(),
        }
    }

    /// Whether the Daemon saved the dismissal of `digest`.
    pub(crate) async fn dismiss(&self, digest: &str) -> bool {
        let Self::Daemon(client) = self else {
            return false;
        };
        match client.sandbox_image_dismiss(digest).await {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(target: "containers.image_update", error = %e, "the Daemon didn't save the dismissal");
                false
            }
        }
    }
}

async fn daemon(
    client: &HttpClient,
    image: &str,
    request: PullRequest,
    flight: Option<u64>,
) -> PullAnswer {
    let answer = match request {
        PullRequest::Poll => client
            .sandbox_image_status(false, image)
            .await
            .map(|status| {
                let offer = ImageOffer::from_status(&status);
                (Some((offer, status.image)), status.pull, status.flight)
            }),
        PullRequest::Start => client
            .sandbox_image_pull(image)
            .await
            .map(|reply| (None, reply.pull, reply.flight)),
        PullRequest::Cancel => client
            .sandbox_image_cancel(image, flight)
            .await
            .map(|reply| (None, reply.pull, None)),
    };
    match answer {
        Ok((polled, pull, flight)) => {
            let (offer, image) = polled.unzip();
            PullAnswer {
                report: PullReport::State(pull),
                offer,
                image,
                flight,
            }
        }
        Err(error) if is_unknown_image(&error) => PullReport::UnknownImage.into(),
        Err(error) if is_code(&error, crate::daemon::ApiErrorCode::PullReplaced) => {
            PullReport::Replaced.into()
        }
        // A 403 is a pull refusal only when the Daemon's own status says it blocks pulls; one from
        // anything else in the way reads as no answer.
        Err(error) if is_refusal(&error) => match client.sandbox_image_status(false, image).await {
            Ok(status) if status.pull_blocked.is_some() => PullReport::Gated {
                command: status.pull_command,
            }
            .into(),
            _ => PullReport::Unavailable.into(),
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

fn is_unknown_image(error: &HttpError) -> bool {
    is_code(error, crate::daemon::ApiErrorCode::UnknownImage)
}

/// The client classifies by the `AoE-Error-Code` header and keeps only that code as `body`.
fn is_code(error: &HttpError, code: crate::daemon::ApiErrorCode) -> bool {
    matches!(error, HttpError::Server { status, body }
        if *status == code.status() && body == code.as_str())
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
    /// Another client's pull found while an outcome waits to be acknowledged: it takes the line
    /// once the outcome is acknowledged, so the outcome isn't lost unread.
    behind: Option<PullLine>,
    /// The Daemon's ids for the running pull shown and the one behind, once an answer named
    /// them; a cancel names the shown one's.
    shown_flight: Option<u64>,
    behind_flight: Option<u64>,
    /// Moves on whenever the line becomes another one, but not on a running pull's progress, so
    /// a key drawn against one outcome can't act on another that looks the same.
    line_id: u64,
}

/// What [`PullWatch::apply`] changed, beyond the line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    Unchanged,
    Changed,
    /// The pull finished, with the digest the Daemon says it installed, if it said.
    Updated(Option<String>),
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

    /// The line as a frame draws it, with its id; none while a running pull is hidden.
    #[cfg(test)]
    pub(crate) fn drawn(&self) -> Option<DrawnPull> {
        self.line().map(|line| DrawnPull {
            line: line.clone(),
            id: self.line_id,
        })
    }

    pub(crate) fn line_id(&self) -> u64 {
        self.line_id
    }

    fn is_drawn(&self, drawn: &DrawnPull) -> bool {
        self.line_id == drawn.id && self.line.as_ref() == Some(&drawn.line)
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
        self.behind = None;
        self.behind_flight = None;
        self.set_line(Some(PullLine::Running(ProgressSnapshot::default())));
        self.watched = true;
        self.seen_running = false;
        self.start_unanswered = false;
    }

    pub(crate) fn apply(&mut self, request: PullRequest, report: PullReport) -> Applied {
        if request == PullRequest::Poll && self.showing_outcome() {
            if let Some(applied) = self.track_behind(&report) {
                return applied;
            }
        }
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
            PullReport::State(PullState::Done { digest }) => {
                self.end(Some(PullLine::Done));
                applied = Applied::Updated(digest);
            }
            PullReport::State(PullState::Cancelled) => self.end(Some(PullLine::Cancelled)),
            PullReport::State(PullState::Failed { reason, .. }) => {
                self.end(Some(PullLine::Failed(reason)))
            }
            // A refused cancel leaves the download running, and hidden if it was.
            PullReport::Gated { .. } if request == PullRequest::Cancel => {
                return Applied::Unanswered(CANCEL_GATED);
            }
            // The pull shown has ended, and what runs now is another one: the next poll shows it.
            PullReport::Replaced => return Applied::Unanswered(CANCEL_REPLACED),
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
                    self.cancel_armed = None;
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
    pub(crate) fn dismiss(&mut self, drawn: &DrawnPull) -> bool {
        if !self.is_drawn(drawn) {
            return false;
        }
        if self.running() {
            self.hidden = true;
        } else {
            self.acknowledge();
        }
        true
    }

    /// The Update key on the drawn line: None when it offers no retry, otherwise whether that
    /// offer is still the line's. A stale offer spends the key without retrying.
    pub(crate) fn retry(&self, drawn: &DrawnPull) -> Option<bool> {
        matches!(drawn.line, PullLine::Failed(_) | PullLine::Cancelled)
            .then(|| self.is_drawn(drawn))
    }

    /// Whether a cancel may go out for this pull. After a Start with no reply, only a poll
    /// showing it running proves it is this TUI's; a cancel, keyed by image, could otherwise
    /// stop another client's pull.
    fn cancellable(&self) -> bool {
        self.running() && !self.start_unanswered
    }

    /// Ctrl+E on a cancellable pull opens the cancel prompt, or restarts its time; it never
    /// cancels, so a held key's repeats can't confirm. Returns whether it opened.
    pub(crate) fn arm_cancel(&mut self, now: Instant) -> bool {
        if !self.cancellable() {
            return false;
        }
        self.cancel_armed = Some(now);
        true
    }

    /// Every key ends the cancel prompt; the key path takes it first, so a key a reader or an
    /// embedded view takes ends it too.
    pub(crate) fn take_cancel_arm(&mut self) -> Option<Instant> {
        self.cancel_armed.take()
    }

    /// Whether the confirming key, pressed `now` with the prompt `armed` before it, cancels.
    pub(crate) fn confirms_cancel(&self, armed: Option<Instant>, now: Instant) -> bool {
        armed.is_some_and(|at| {
            self.cancellable() && now.saturating_duration_since(at) <= CANCEL_CONFIRM_TTL
        })
    }

    /// The cancel prompt is open `now`: the bar draws it over any other line.
    pub(crate) fn cancel_prompt(&self, now: Instant) -> bool {
        self.confirms_cancel(self.cancel_armed, now)
    }

    /// A key that isn't the pull's clears the Done line the last frame drew, so the key acts as
    /// usual. Returns whether the line changed.
    pub(crate) fn other_key(&mut self, drawn: Option<&DrawnPull>) -> bool {
        if drawn.is_some_and(|d| d.line == PullLine::Done && self.is_drawn(d)) {
            self.acknowledge();
            return true;
        }
        false
    }

    /// The pull id the answer to the last request carried. A running pull with a new id is
    /// another pull than the one shown: a cancel prompt for it ends, and a hide doesn't carry
    /// over. Returns whether the bar changed.
    pub(crate) fn note_flight(&mut self, flight: Option<u64>) -> bool {
        let Some(flight) = flight else {
            return false;
        };
        if self.showing_outcome() && self.behind.is_some() {
            self.behind_flight = Some(flight);
            return false;
        }
        if !self.running() {
            return false;
        }
        // Our own pull's first id is the one it started.
        let replaced = self.shown_flight.is_some_and(|shown| shown != flight);
        self.shown_flight = Some(flight);
        if !replaced {
            return false;
        }
        self.line_id += 1;
        let unhid = std::mem::take(&mut self.hidden);
        self.cancel_armed.take().is_some() || unhid
    }

    /// The id of the running pull shown, for the cancel to name.
    pub(crate) fn cancel_flight(&self) -> Option<u64> {
        self.shown_flight
    }

    fn showing_outcome(&self) -> bool {
        matches!(
            self.line,
            Some(PullLine::Done | PullLine::Failed(_) | PullLine::Cancelled)
        )
    }

    /// Another client's pull waits behind the outcome shown, still running.
    pub(crate) fn behind_running(&self) -> bool {
        matches!(self.behind, Some(PullLine::Running(_)))
    }

    /// While an outcome is shown, a poll reports another client's pull, which waits behind it.
    /// None when the report isn't about such a pull, so it applies as usual; `Updated` when
    /// that pull finished, so its install is recorded though its Done shows later.
    fn track_behind(&mut self, report: &PullReport) -> Option<Applied> {
        match report {
            PullReport::State(PullState::Running(snapshot)) => {
                self.behind = Some(PullLine::Running(*snapshot));
            }
            PullReport::State(PullState::Done { digest }) if self.behind_running() => {
                self.behind = Some(PullLine::Done);
                self.behind_flight = None;
                return Some(Applied::Updated(digest.clone()));
            }
            PullReport::State(state) if self.behind.is_some() => {
                // An ended or vanished pull's id goes with it.
                self.behind_flight = None;
                self.behind = match state {
                    PullState::Done { .. } => Some(PullLine::Done),
                    PullState::Cancelled => Some(PullLine::Cancelled),
                    PullState::Failed { reason, .. } => Some(PullLine::Failed(*reason)),
                    _ => None,
                };
            }
            _ => return None,
        }
        Some(Applied::Unchanged)
    }

    /// The shown outcome is acknowledged: a pull waiting behind it takes the line.
    fn acknowledge(&mut self) {
        let next = self.behind.take();
        let flight = std::mem::take(&mut self.behind_flight);
        let running = matches!(next, Some(PullLine::Running(_)));
        self.watched = running;
        self.seen_running = running;
        self.start_unanswered = false;
        self.set_line(next);
        if running {
            self.shown_flight = flight;
        }
    }

    fn end(&mut self, outcome: Option<PullLine>) {
        self.watched = false;
        self.seen_running = false;
        self.start_unanswered = false;
        self.set_line(outcome);
    }

    fn set_line(&mut self, line: Option<PullLine>) {
        let progress = self.running() && matches!(line, Some(PullLine::Running(_)));
        if !progress {
            self.line_id += 1;
        }
        if !matches!(line, Some(PullLine::Running(_))) {
            self.hidden = false;
            self.cancel_armed = None;
            self.shown_flight = None;
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
        let done = || {
            PullReport::State(PullState::Done {
                digest: Some("sha256:d".into()),
            })
        };
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
                Applied::Updated(Some("sha256:d".into())),
            ),
            (
                "started, then done",
                true,
                vec![(Poll, done())],
                shown(PullLine::Done),
                Applied::Updated(Some("sha256:d".into())),
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
                Applied::Updated(Some("sha256:d".into())),
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

    /// Another client's pull found while an outcome is unacknowledged waits behind it, and
    /// takes the line once the outcome is acknowledged; a retry here replaces both at once.
    #[test]
    fn an_unacknowledged_outcome_keeps_the_line_from_another_clients_pull() {
        let failure = PullLine::Failed(image_pull::PullFailureReason::Network);
        let other = |done| match running(done) {
            PullReport::State(PullState::Running(snapshot)) => PullLine::Running(snapshot),
            _ => unreachable!(),
        };
        // (case, the first outcome, reports after it, the line those leave, the line after the
        // outcome is acknowledged)
        let cases: Vec<(
            &str,
            PullReport,
            Vec<PullReport>,
            PullLine,
            Option<PullLine>,
        )> = vec![
            (
                "a failure, then another pull",
                failed(),
                vec![running(1)],
                failure.clone(),
                Some(other(1)),
            ),
            (
                "a failure, then another pull that finishes",
                failed(),
                vec![
                    running(1),
                    PullReport::State(PullState::Done {
                        digest: Some("sha256:d".into()),
                    }),
                ],
                failure.clone(),
                Some(PullLine::Done),
            ),
            (
                "done, then another pull",
                PullReport::State(PullState::Done { digest: None }),
                vec![running(2)],
                PullLine::Done,
                Some(other(2)),
            ),
            (
                "a failure, and nothing after",
                failed(),
                vec![],
                failure.clone(),
                None,
            ),
        ];
        for (case, outcome, after, shown, acknowledged) in cases {
            let mut watch = PullWatch::default();
            watch.start();
            watch.apply(PullRequest::Poll, outcome);
            for report in after {
                let finished = report
                    == PullReport::State(PullState::Done {
                        digest: Some("sha256:d".into()),
                    });
                assert_eq!(
                    watch.apply(PullRequest::Poll, report),
                    if finished {
                        Applied::Updated(Some("sha256:d".into()))
                    } else {
                        Applied::Unchanged
                    },
                    "{case}"
                );
            }
            assert_eq!(
                watch.line(),
                Some(&shown),
                "{case}: the outcome was replaced"
            );
            if shown == PullLine::Done {
                assert!(watch.other_key(watch.drawn().as_ref()), "{case}");
            } else {
                assert!(watch.dismiss(&watch.drawn().unwrap()), "{case}");
            }
            assert_eq!(watch.line(), acknowledged.as_ref(), "{case}: acknowledged");
        }

        // A retry here is this TUI's own pull: it replaces the outcome and what waited behind it.
        let mut watch = PullWatch::default();
        watch.start();
        watch.apply(PullRequest::Poll, failed());
        watch.apply(PullRequest::Poll, running(1));
        watch.start();
        assert_eq!(
            watch.line(),
            Some(&PullLine::Running(ProgressSnapshot::default()))
        );
        watch.apply(
            PullRequest::Poll,
            PullReport::State(PullState::Done { digest: None }),
        );
        assert_eq!(watch.line(), Some(&PullLine::Done), "the retry's outcome");
    }

    /// Ctrl+X hides a running pull, whose progress stays for the key line, until it ends;
    /// on an outcome it acknowledges. A line no longer current is left alone.
    #[test]
    fn dismiss_hides_a_running_pull_and_acknowledges_an_outcome() {
        let mut watch = PullWatch::default();
        watch.apply(PullRequest::Poll, running(3));
        let drawn = watch.drawn().unwrap();
        watch.apply(PullRequest::Poll, running(4));
        assert!(!watch.dismiss(&drawn), "a line since redrawn was dismissed");
        let drawn = watch.drawn().unwrap();
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
            .drawn()
            .expect("the outcome shows though the pull was hidden");
        assert_eq!(watch.hidden_progress(), None);
        assert_eq!(watch.retry(&outcome), Some(true));
        assert!(watch.dismiss(&outcome));
        assert_eq!(watch.line(), None);
        assert!(!watch.holds_line());

        // A hide is of one pull: another id that replaces it shows, but its own first doesn't.
        watch.apply(PullRequest::Poll, running(1));
        let drawn = watch.drawn().unwrap();
        assert!(watch.dismiss(&drawn));
        assert!(
            !watch.note_flight(Some(1)),
            "the pull's first id changed the bar"
        );
        assert_eq!(watch.line(), None, "the pull's first id unhid it");
        assert!(!watch.note_flight(Some(1)), "the same pull changed the bar");
        assert_eq!(watch.line(), None, "the same pull unhid");
        assert!(watch.note_flight(Some(2)), "another pull didn't repaint");
        assert!(watch.line().is_some(), "another pull started hidden");
    }

    /// Ctrl+E only opens the prompt, so repeating it never cancels; the confirming key cancels
    /// within the prompt's time, and the prompt shows only while it can.
    #[test]
    fn ctrl_e_asks_and_only_the_confirming_key_cancels() {
        let at = Instant::now();
        let mut watch = PullWatch::default();
        assert!(!watch.arm_cancel(at), "nothing to cancel");
        let armed = watch.take_cancel_arm();
        assert!(!watch.confirms_cancel(armed, at), "no prompt to confirm");
        watch.start();
        for (confirm, expected) in [
            (at + CANCEL_CONFIRM_TTL, true),
            (at + CANCEL_CONFIRM_TTL + Duration::from_millis(1), false),
        ] {
            for repeat in 0..5 {
                assert!(watch.arm_cancel(at), "Ctrl+E #{repeat} didn't ask");
            }
            assert_eq!(watch.cancel_prompt(confirm), expected, "{:?}", confirm - at);
            let armed = watch.take_cancel_arm();
            assert_eq!(
                watch.confirms_cancel(armed, confirm),
                expected,
                "{:?}",
                confirm - at
            );
            assert!(!watch.cancel_prompt(at), "the prompt outlived its answer");
        }
        assert!(watch.arm_cancel(at));
        assert!(
            !watch.note_flight(Some(3)),
            "our pull's first id changed the bar"
        );
        assert!(
            watch.cancel_prompt(at),
            "our pull's first id ended the prompt"
        );
        assert!(
            !watch.other_key(watch.drawn().as_ref()),
            "a running pull's line changed on a key"
        );

        watch.apply(
            PullRequest::Poll,
            PullReport::State(PullState::Done { digest: None }),
        );
        assert!(
            !watch.other_key(None),
            "a key cleared a Done that wasn't drawn"
        );
        assert!(watch.other_key(watch.drawn().as_ref()));
        assert_eq!(watch.line(), None);

        // A Start with no reply ends the prompt, so a poll then showing a pull running, under
        // an id this TUI never saw and maybe another client's, doesn't bring it back.
        watch.start();
        assert!(watch.arm_cancel(at));
        watch.apply(PullRequest::Start, PullReport::Unavailable);
        watch.apply(PullRequest::Poll, running(1));
        assert!(!watch.note_flight(Some(7)));
        assert_eq!(watch.cancel_flight(), Some(7));
        assert!(watch.cancellable());
        assert!(!watch.cancel_prompt(at), "the prompt came back on a poll");
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
                flight: None,
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

    /// A Daemon that won't pull reports the command to run instead. A 403 its status doesn't
    /// explain, as from a proxy in front of it, is no answer rather than a refusal.
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
        for read_only in [true, false] {
            let state = test_support::build_test_app_state_configured(Vec::new(), |s| {
                s.read_only = read_only
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let daemon = test_support::build_router_for_test(state.clone());
            let app = if read_only {
                daemon
            } else {
                let forbidden = || async { axum::http::StatusCode::FORBIDDEN };
                axum::Router::new()
                    .route("/api/sandbox/image-pull", axum::routing::post(forbidden))
                    .route(
                        "/api/sandbox/image-pull/cancel",
                        axum::routing::post(forbidden),
                    )
                    .fallback_service(daemon)
            };
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
                source.run(IMAGE, PullRequest::Poll, None).await,
                PullAnswer {
                    report: PullReport::State(PullState::Idle),
                    offer: Some(ImageOffer::NoUpdate),
                    image: Some(IMAGE.into()),
                    flight: None,
                },
                "checks are off, so the Daemon offers nothing"
            );
            for request in [PullRequest::Start, PullRequest::Cancel] {
                let report = source.run(IMAGE, request, None).await.report;
                if read_only {
                    let PullReport::Gated { command } = report else {
                        panic!("{request:?} should be gated");
                    };
                    assert!(command.contains(IMAGE), "{command}");
                } else {
                    assert_eq!(
                        report,
                        PullReport::Unavailable,
                        "{request:?}: a 403 the Daemon's status doesn't explain was gated"
                    );
                }
            }
            state.shutdown.cancel();
            server.await.unwrap();
        }
    }

    /// Ctrl+X's dismissal through the real router, kept by the Daemon whichever transport
    /// reaches it. A refusal and a lost Daemon both say it wasn't saved.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_daemon_keeps_a_dismissal_over_either_transport() {
        let _app = crate::session::test_support::isolate_app_dir();
        let digest = format!("sha256:{}", "0a".repeat(32));
        let stored = || {
            crate::session::config::AppStateConfig::load()
                .unwrap()
                .dismissed_image_digest
        };
        let tcp = |state: std::sync::Arc<crate::server::AppState>, token: Option<&str>| {
            let token = token.map(str::to_string);
            async move {
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
                let endpoint = DaemonEndpoint::new(base, token, crate::daemon::Source::Env);
                (
                    PullSource::Daemon(HttpClient::new(endpoint).unwrap()),
                    server,
                )
            }
        };

        // Same machine: the owner-checked Unix socket the local Daemon serves, in the app dir.
        let socket = crate::session::get_app_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("daemon/api.sock");
        // The Unix client sends `Host: localhost`, which the real Daemon allows.
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
        );
        let server = test_support::serve_owner_router_for_test(
            &socket,
            test_support::build_router_for_test(state.clone()),
        )
        .await;
        let local =
            PullSource::Daemon(HttpClient::new(DaemonEndpoint::local_unix(socket, None)).unwrap());
        assert!(local.dismiss(&digest).await, "same machine");
        assert_eq!(stored(), Some(digest.clone()), "same machine");
        server.abort();

        // Remote: `AOE_DAEMON_URL` with a token.
        crate::session::update_app_state(|s| s.dismissed_image_digest = None).unwrap();
        let token = "dismiss-token";
        let state = test_support::build_test_app_state_with_policy(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some(token.into()),
        );
        let (remote, server) = tcp(state.clone(), Some(token)).await;
        assert!(remote.dismiss(&digest).await, "remote");
        assert_eq!(stored(), Some(digest.clone()), "remote");
        state.shutdown.cancel();
        server.await.unwrap();

        crate::session::update_app_state(|s| s.dismissed_image_digest = None).unwrap();
        let read_only =
            test_support::build_test_app_state_configured(Vec::new(), |s| s.read_only = true);
        let (refused, server) = tcp(read_only.clone(), None).await;
        assert!(
            !refused.dismiss(&digest).await,
            "a read-only Daemon answers 403"
        );
        read_only.shutdown.cancel();
        server.await.unwrap();

        // A port this test keeps, where each connection is dropped unanswered.
        let hangup = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", hangup.local_addr().unwrap());
        let hangups = tokio::spawn(async move {
            while let Ok((connection, _)) = hangup.accept().await {
                drop(connection);
            }
        });
        let gone = PullSource::Daemon(
            HttpClient::new(DaemonEndpoint::new(base, None, crate::daemon::Source::Env)).unwrap(),
        );
        assert!(!gone.dismiss(&digest).await, "transport error");
        hangups.abort();
        assert!(!PullSource::Unavailable.dismiss(&digest).await, "no Daemon");
        assert_eq!(stored(), None, "a refused dismissal wrote nothing");
    }
}
