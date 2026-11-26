//! Runs a sandbox login for the TUI on a thread of its own.
//!
//! `LoginHelper` is async and owns a child process, so the whole login lives on one
//! thread with a private runtime. The UI sends the code in and polls events out.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::session::sandbox_login::code::LoginCode;
use crate::session::sandbox_login::helper::{LoginError, LoginHelper};

/// How long quitting waits for a cancelled helper to be cleaned up.
const QUIT_WAIT: Duration = Duration::from_secs(5);

/// Why a login could not start, which decides what the dialog offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailure {
    /// The credential is already usable; a forced login replaces it.
    AlreadyOk,
    /// The setting is off; starting again cannot help.
    NotSeparateLogin,
    /// The container runtime cannot run the helper; starting again cannot help.
    Unsupported,
    Retry,
}

impl StartFailure {
    fn of(error: &LoginError) -> Self {
        match error {
            LoginError::AlreadyLoggedIn => Self::AlreadyOk,
            LoginError::NotSeparateLogin => Self::NotSeparateLogin,
            LoginError::Unsupported(_) => Self::Unsupported,
            _ => Self::Retry,
        }
    }
}

pub enum LoginEvent {
    Started {
        url: String,
    },
    StartFailed {
        message: String,
        failure: StartFailure,
    },
    /// The login is over without success (rejected code, timeout, client exit).
    Ended {
        message: String,
    },
    LoggedIn,
}

enum Command {
    Submit(LoginCode),
    Cancel,
}

/// The helper of a start in progress, owned from its spawn on, so a cancel reaches it while
/// it waits for the sign-in URL.
pub(in crate::tui) struct Starting {
    helper: Mutex<Option<Arc<LoginHelper>>>,
    cancelled: Arc<AtomicBool>,
    /// Ends the preparation of a start, the image pull included.
    token: CancellationToken,
}

/// The one deadline quitting gives all the cleanups of login helpers, however many there
/// are: it starts with the first wait.
#[derive(Clone, Default)]
pub(in crate::tui) struct QuitClock(Arc<OnceLock<Instant>>);

impl QuitClock {
    fn remaining(&self) -> Duration {
        let began = *self.0.get_or_init(Instant::now);
        (began + QUIT_WAIT).saturating_duration_since(Instant::now())
    }

    /// Waits for `done` to disconnect, at most until the deadline. A thread that has ended
    /// already neither waits nor starts the clock.
    fn wait(&self, done: &mpsc::Receiver<()>) {
        if !matches!(done.try_recv(), Err(TryRecvError::Disconnected)) {
            let _ = done.recv_timeout(self.remaining());
        }
    }
}

/// A cancelled login whose helper is still being cleaned up. Dropping it waits for that,
/// within the quit deadline; closing the dialog holds it, so a quit that follows does too.
pub(in crate::tui) struct Retiring {
    done: mpsc::Receiver<()>,
    clock: QuitClock,
}

impl Retiring {
    pub(in crate::tui) fn is_done(&self) -> bool {
        matches!(self.done.try_recv(), Err(TryRecvError::Disconnected))
    }
}

impl Drop for Retiring {
    fn drop(&mut self) {
        self.clock.wait(&self.done);
    }
}

impl Starting {
    /// Spawns a helper unless the login was cancelled. The flag is set before a cancel
    /// command is sent, and checked here under the lock `take` also holds: the cancel finds
    /// this helper or this finds the flag.
    pub(in crate::tui) fn admit(
        &self,
        spawn: &mut dyn FnMut() -> Result<Arc<LoginHelper>, LoginError>,
    ) -> Result<Arc<LoginHelper>, LoginError> {
        let mut slot = self.helper.lock().unwrap_or_else(PoisonError::into_inner);
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(LoginError::Cancelled);
        }
        let helper = spawn()?;
        *slot = Some(Arc::clone(&helper));
        Ok(helper)
    }

    fn take(&self) -> Option<Arc<LoginHelper>> {
        self.helper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

pub struct SandboxLoginPoller {
    commands: UnboundedSender<Command>,
    cancelled: Arc<AtomicBool>,
    token: CancellationToken,
    events: mpsc::Receiver<LoginEvent>,
    /// Disconnects when the thread is done.
    done: mpsc::Receiver<()>,
    /// Set by `cancel`: dropping then does not wait for the thread.
    detached: bool,
    clock: QuitClock,
}

impl SandboxLoginPoller {
    /// Logs in the file a launch of `tool` and its own `command` waits on under `profile`.
    pub fn spawn(profile: String, tool: String, command: Option<String>, force: bool) -> Self {
        Self::spawn_registered(move |starting| async move {
            LoginHelper::start_admitted(
                &profile,
                &tool,
                command.as_deref(),
                force,
                &starting.token,
                &|spawn| starting.admit(spawn),
            )
            .await
        })
    }

    /// Shares the quit deadline with the other logins of the view.
    pub(in crate::tui) fn with_clock(mut self, clock: QuitClock) -> Self {
        self.clock = clock;
        self
    }

    pub(in crate::tui) fn spawn_with(
        start: impl Future<Output = Result<Arc<LoginHelper>, LoginError>> + Send + 'static,
    ) -> Self {
        Self::spawn_registered(|_| start)
    }

    /// `start` gets the slot its helper has to be admitted through, to be cancellable while
    /// it waits for its URL.
    pub(in crate::tui) fn spawn_registered<F>(
        start: impl FnOnce(Arc<Starting>) -> F + Send + 'static,
    ) -> Self
    where
        F: Future<Output = Result<Arc<LoginHelper>, LoginError>>,
    {
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::channel();
        let (done_tx, done) = mpsc::channel::<()>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let token = CancellationToken::new();
        let starting = Arc::new(Starting {
            helper: Mutex::new(None),
            cancelled: Arc::clone(&cancelled),
            token: token.clone(),
        });
        let failed = event_tx.clone();
        let run = move || {
            let _done = done_tx;
            run(
                start(Arc::clone(&starting)),
                command_rx,
                flag,
                event_tx,
                starting,
            );
        };
        if let Err(e) = std::thread::Builder::new()
            .name("aoe-sandbox-login".to_string())
            .spawn(run)
        {
            tracing::warn!(target: "tui.sandbox_login", error = %e, "login thread spawn failed");
            let _ = failed.send(LoginEvent::StartFailed {
                message: format!("could not start the login worker: {e}"),
                failure: StartFailure::Retry,
            });
        }
        Self {
            commands,
            cancelled,
            token,
            events,
            done,
            detached: false,
            clock: QuitClock::default(),
        }
    }

    pub fn submit(&self, code: LoginCode) {
        let _ = self.commands.send(Command::Submit(code));
    }

    /// Cancel without waiting: the thread kills the helper and cleans up on its own. The
    /// returned handle waits for that when it is dropped, within the quit deadline; the
    /// caller keeps it until then.
    #[must_use = "dropping the handle waits for the cleanup"]
    pub(in crate::tui) fn cancel(mut self) -> Retiring {
        self.detached = true;
        self.request_cancel();
        let (_, ended) = mpsc::channel::<()>();
        Retiring {
            done: std::mem::replace(&mut self.done, ended),
            clock: self.clock.clone(),
        }
    }

    fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.token.cancel();
        let _ = self.commands.send(Command::Cancel);
    }

    pub fn try_recv(&self) -> Result<LoginEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Blocks until the login thread has ended.
    #[cfg(test)]
    pub(in crate::tui) fn wait_done_for_test(&self) {
        assert!(matches!(
            self.done
                .recv_timeout(crate::session::sandbox_login::test_fake::DEADLINE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[cfg(test)]
    pub(crate) fn recv_for_test(&self) -> LoginEvent {
        self.events
            .recv_timeout(crate::session::sandbox_login::test_fake::DEADLINE)
            .expect("no login event before the deadline")
    }
}

impl Drop for SandboxLoginPoller {
    /// Quitting leaves no helper container behind, within the quit deadline; the next login
    /// start sweeps by label what a slow removal leaves.
    fn drop(&mut self) {
        self.request_cancel();
        if !self.detached {
            self.clock.wait(&self.done);
        }
    }
}

/// The helper's error text is already free of the code and the link's `state`.
fn ended_message(error: &LoginError) -> String {
    match error {
        LoginError::Gone => "That login ended. Start again for a new link.".to_string(),
        LoginError::Failed { message } => message.clone(),
        other => other.to_string(),
    }
}

fn start_message(error: &LoginError) -> String {
    match error {
        LoginError::AlreadyLoggedIn => "The sandbox login is already usable.".to_string(),
        other => other.to_string(),
    }
}

fn run(
    start: impl Future<Output = Result<Arc<LoginHelper>, LoginError>>,
    commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    cancelled: Arc<AtomicBool>,
    events: mpsc::Sender<LoginEvent>,
    starting: Arc<Starting>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = events.send(LoginEvent::StartFailed {
                message: format!("could not start the login worker: {e}"),
                failure: StartFailure::Retry,
            });
            return;
        }
    };
    runtime.block_on(drive(start, commands, &cancelled, &events, &starting));
    // Nothing the login needs is left on the blocking pool; do not wait for the rest.
    runtime.shutdown_background();
}

async fn drive(
    start: impl Future<Output = Result<Arc<LoginHelper>, LoginError>>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    cancelled: &AtomicBool,
    events: &mpsc::Sender<LoginEvent>,
    starting: &Starting,
) {
    // A cancel during the start still lets it return, since dropping it mid-way could orphan
    // a container. It ends a helper that already runs, so the URL wait does not hold the
    // credential lock, and stops an image pull, which leaves no container and frees the lock.
    tokio::pin!(start);
    let mut cancelling = false;
    let mut early_code = None;
    let started = loop {
        tokio::select! {
            result = &mut start => break result,
            command = commands.recv(), if !cancelling => match command {
                Some(Command::Submit(code)) => {
                    early_code.get_or_insert(code);
                }
                Some(Command::Cancel) | None => {
                    cancelling = true;
                    if let Some(helper) = starting.take() {
                        helper.cancel().await;
                    }
                }
            },
        }
    };
    let helper = match started {
        Ok(helper) => helper,
        Err(error) => {
            if !cancelled.load(Ordering::SeqCst) {
                let _ = events.send(LoginEvent::StartFailed {
                    message: start_message(&error),
                    failure: StartFailure::of(&error),
                });
            }
            return;
        }
    };
    if cancelled.load(Ordering::SeqCst) {
        helper.cancel().await;
        return;
    }
    let _ = events.send(LoginEvent::Started {
        url: helper.url().to_string(),
    });
    let code = match early_code {
        Some(code) => code,
        None => tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Submit(code)) => code,
                Some(Command::Cancel) | None => {
                    helper.cancel().await;
                    return;
                }
            },
            ended = helper.ended() => {
                let _ = events.send(LoginEvent::Ended { message: ended_message(&ended) });
                helper.cancel().await;
                return;
            }
        },
    };
    let submit = helper.submit_code(code);
    tokio::pin!(submit);
    let outcome = loop {
        tokio::select! {
            outcome = &mut submit => break outcome,
            // One code per helper: a second one is dropped (and so wiped), not an abort.
            command = commands.recv() => match command {
                Some(Command::Submit(_)) => {}
                Some(Command::Cancel) | None => {
                    helper.cancel().await;
                    return;
                }
            },
        }
    };
    match outcome {
        Ok(()) => {
            let _ = events.send(LoginEvent::LoggedIn);
        }
        Err(error) => {
            // A rejected code leaves the client running; the CLI cancels here too.
            helper.cancel().await;
            let _ = events.send(LoginEvent::Ended {
                message: ended_message(&error),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::sandbox_login::claude::LOGIN;
    use crate::session::sandbox_login::helper::HelperTimeouts;
    use crate::session::sandbox_login::test_fake::{Fake, MEASURED_URL};

    fn code(raw: &str) -> LoginCode {
        LoginCode::parse(raw.to_string()).unwrap()
    }

    fn poller_for(fake: &Fake) -> SandboxLoginPoller {
        poller_timed(fake, HelperTimeouts::default())
    }

    fn poller_timed(fake: &Fake, timeouts: HelperTimeouts) -> SandboxLoginPoller {
        let argv = fake.argv(true);
        let shared = fake.shared.clone();
        let cleanup = fake.cleanup();
        SandboxLoginPoller::spawn_with(async move {
            LoginHelper::start_with(&LOGIN, argv, shared, cleanup, timeouts).await
        })
    }

    fn ended(event: LoginEvent) -> String {
        match event {
            LoginEvent::Ended { message } => message,
            _ => panic!("expected the login to end"),
        }
    }

    #[test]
    fn sandbox_login_poller_logs_in_with_a_good_code() {
        let fake = Fake::new();
        let poller = poller_for(&fake);
        match poller.recv_for_test() {
            LoginEvent::Started { url } => assert_eq!(url, MEASURED_URL),
            _ => panic!("expected the link first"),
        }
        poller.submit(code("good-code#good-state"));
        assert!(matches!(poller.recv_for_test(), LoginEvent::LoggedIn));
    }

    #[test]
    fn sandbox_login_poller_reports_a_rejected_code_as_an_ended_login() {
        for (typed, want) in [
            ("invalid-code", "Invalid code"),
            ("wrong-code", "Login failed"),
        ] {
            let fake = Fake::new();
            let poller = poller_for(&fake);
            assert!(matches!(poller.recv_for_test(), LoginEvent::Started { .. }));
            poller.submit(code(typed));
            let message = ended(poller.recv_for_test());
            assert!(message.contains(want), "{typed}: {message}");
            assert!(!message.contains(typed), "{typed}: {message}");
            assert!(!message.contains("state=SAMPLEstate"), "{message}");
        }
    }

    #[test]
    fn sandbox_login_poller_drop_kills_the_waiting_helper_and_cleans_up() {
        let fake = Fake::new();
        let poller = poller_for(&fake);
        assert!(matches!(poller.recv_for_test(), LoginEvent::Started { .. }));
        drop(poller);
        assert!(fake.cleaned().exists());
    }

    #[test]
    fn sandbox_login_poller_cancel_before_the_helper_is_up_cancels_it_when_it_arrives() {
        let fake = Fake::new();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (argv, shared, cleanup) = (fake.argv(true), fake.shared.clone(), fake.cleanup());
        let poller = SandboxLoginPoller::spawn_with(async move {
            let _ = released.await;
            LoginHelper::start_with(&LOGIN, argv, shared, cleanup, HelperTimeouts::default()).await
        });
        poller.request_cancel();
        release.send(()).unwrap();
        // The thread ends without announcing a link, and the helper is gone.
        assert!(matches!(
            poller
                .done
                .recv_timeout(crate::session::sandbox_login::test_fake::DEADLINE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(matches!(poller.try_recv(), Err(TryRecvError::Disconnected)));
        assert!(fake.cleaned().exists());
    }

    #[test]
    fn sandbox_login_poller_cancel_reaches_a_helper_still_waiting_for_its_url() {
        let fake = Fake::new();
        let (started_tx, started) = mpsc::channel();
        let (shared, cleanup) = (fake.shared.clone(), fake.cleanup());
        let poller = SandboxLoginPoller::spawn_registered(move |starting| async move {
            LoginHelper::start_with_admission(
                &LOGIN,
                // Runs, but never shows a sign-in URL.
                vec!["sleep".into(), "3600".into()],
                shared,
                cleanup,
                HelperTimeouts::default(),
                &|spawn| {
                    let helper = starting.admit(spawn)?;
                    let _ = started_tx.send(());
                    Ok(helper)
                },
            )
            .await
        });
        started
            .recv_timeout(crate::session::sandbox_login::test_fake::DEADLINE)
            .expect("the helper started");

        poller.request_cancel();

        // Long before the URL wait (90 s) would end on its own.
        assert!(matches!(
            poller
                .done
                .recv_timeout(crate::session::sandbox_login::test_fake::DEADLINE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(matches!(poller.try_recv(), Err(TryRecvError::Disconnected)));
        assert!(fake.cleaned().exists());
        // The credential lock is free for the next login.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let next = runtime
            .block_on(fake.start(true))
            .expect("the credential lock is free");
        runtime.block_on(next.cancel());
    }

    #[test]
    fn sandbox_login_poller_a_cancel_before_the_spawn_refuses_the_helper() {
        let fake = Fake::new();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (spawned_tx, spawned) = mpsc::channel();
        let (shared, cleanup) = (fake.shared.clone(), fake.cleanup());
        let poller = SandboxLoginPoller::spawn_registered(move |starting| async move {
            // The image pull of a real start.
            let _ = released.await;
            LoginHelper::start_with_admission(
                &LOGIN,
                vec!["sleep".into(), "3600".into()],
                shared,
                cleanup,
                HelperTimeouts::default(),
                &|spawn| {
                    let helper = starting.admit(spawn)?;
                    let _ = spawned_tx.send(());
                    Ok(helper)
                },
            )
            .await
        });
        poller.request_cancel();
        release.send(()).unwrap();

        assert!(matches!(
            poller
                .done
                .recv_timeout(crate::session::sandbox_login::test_fake::DEADLINE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(spawned.try_recv().is_err(), "no helper was spawned");
        assert!(!fake.cleaned().exists());
    }

    #[test]
    fn sandbox_login_poller_start_failures_say_what_to_do() {
        for (error, want) in [
            (LoginError::AlreadyLoggedIn, StartFailure::AlreadyOk),
            (LoginError::NotSeparateLogin, StartFailure::NotSeparateLogin),
            (
                LoginError::Unsupported(crate::session::SandboxLoginUnsupported),
                StartFailure::Unsupported,
            ),
            (LoginError::Busy, StartFailure::Retry),
        ] {
            let text = error.to_string();
            let poller = SandboxLoginPoller::spawn_with(async move { Err(error) });
            match poller.recv_for_test() {
                LoginEvent::StartFailed { message, failure } => {
                    assert_eq!(failure, want, "{text}");
                    assert!(!message.contains("--force"), "{message}");
                    if want != StartFailure::AlreadyOk {
                        assert_eq!(message, text);
                    }
                }
                _ => panic!("expected a start failure"),
            }
        }
    }

    #[test]
    fn sandbox_login_poller_cancel_returns_promptly_while_the_start_is_blocked() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let poller = SandboxLoginPoller::spawn_with(async move {
            let _ = released.await;
            Err(LoginError::Gone)
        });
        let began = std::time::Instant::now();
        let _retiring = poller.cancel();
        assert!(
            began.elapsed() < Duration::from_millis(500),
            "cancel blocked for {:?}",
            began.elapsed()
        );
        let _ = release.send(());
    }

    #[test]
    fn sandbox_login_poller_a_stray_second_code_does_not_cancel_the_login() {
        let fake = Fake::new();
        let timeouts = HelperTimeouts {
            after_code: Duration::from_millis(1500),
            ..HelperTimeouts::default()
        };
        let poller = poller_timed(&fake, timeouts);
        assert!(matches!(poller.recv_for_test(), LoginEvent::Started { .. }));
        poller.submit(code("hang-first"));
        poller.submit(code("hang-second"));
        // The first code is still being answered: the wait ends with its own timeout,
        // not with a silent cancel (which would end the stream without an event).
        let message = ended(poller.recv_for_test());
        assert_eq!(message, LoginError::Timeout.to_string());
    }

    #[test]
    fn sandbox_login_poller_a_dead_thread_disconnects_the_events() {
        let poller = SandboxLoginPoller::spawn_with(async { panic!("login thread died") });
        poller.wait_done_for_test();
        assert!(matches!(poller.try_recv(), Err(TryRecvError::Disconnected)));
    }

    #[test]
    fn sandbox_login_poller_an_ended_login_gets_the_friendly_text() {
        assert_eq!(
            ended_message(&LoginError::Gone),
            "That login ended. Start again for a new link."
        );
        assert_eq!(
            ended_message(&LoginError::Failed {
                message: "nope".to_string()
            }),
            "nope"
        );
        assert_eq!(
            ended_message(&LoginError::Timeout),
            LoginError::Timeout.to_string()
        );
    }
}
