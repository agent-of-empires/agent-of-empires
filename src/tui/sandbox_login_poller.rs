//! Runs a sandbox login for the TUI on a thread of its own.
//!
//! `LoginHelper` is async and owns a child process, so the whole login lives on one
//! thread with a private runtime. The UI sends the code in and polls events out.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

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

pub struct SandboxLoginPoller {
    commands: UnboundedSender<Command>,
    cancelled: Arc<AtomicBool>,
    events: mpsc::Receiver<LoginEvent>,
    /// Disconnects when the thread is done.
    done: mpsc::Receiver<()>,
    /// Set by `cancel`: dropping then does not wait for the thread.
    detached: bool,
}

impl SandboxLoginPoller {
    /// Logs in the file a launch of `tool` and its own `command` waits on under `profile`.
    pub fn spawn(profile: String, tool: String, command: Option<String>, force: bool) -> Self {
        Self::spawn_with(async move {
            LoginHelper::start(&profile, &tool, command.as_deref(), force).await
        })
    }

    pub(in crate::tui) fn spawn_with(
        start: impl Future<Output = Result<Arc<LoginHelper>, LoginError>> + Send + 'static,
    ) -> Self {
        let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::channel();
        let (done_tx, done) = mpsc::channel::<()>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let failed = event_tx.clone();
        let run = move || {
            let _done = done_tx;
            run(start, command_rx, flag, event_tx);
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
            events,
            done,
            detached: false,
        }
    }

    pub fn submit(&self, code: LoginCode) {
        let _ = self.commands.send(Command::Submit(code));
    }

    /// Cancel without waiting, even on drop: the thread kills the helper and cleans up
    /// on its own. Only dropping a poller that was not cancelled waits (bounded).
    pub fn cancel(mut self) {
        self.detached = true;
        self.request_cancel();
    }

    fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
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
    /// Quitting leaves no helper container behind, within `QUIT_WAIT`. A start that is
    /// still in flight cannot be interrupted (see `drive`), so this can run out of time;
    /// the next login start sweeps such a container by its label.
    fn drop(&mut self) {
        self.request_cancel();
        if !self.detached {
            let _ = self.done.recv_timeout(QUIT_WAIT);
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
    runtime.block_on(drive(start, commands, &cancelled, &events));
    // A cancelled image pull may still be running on the blocking pool.
    runtime.shutdown_background();
}

async fn drive(
    start: impl Future<Output = Result<Arc<LoginHelper>, LoginError>>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    cancelled: &AtomicBool,
    events: &mpsc::Sender<LoginEvent>,
) {
    // A cancel during the start waits for it: dropping it mid-way could orphan a container.
    // Quitting does not wait that long, so a container can still outlive the app; the next
    // login start sweeps it by its label.
    let helper = match start.await {
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
    let code = tokio::select! {
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
        poller.cancel();
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
