//! Reads, starts and cancels the sandbox image pull for the home screen: through the Daemon when
//! one is running, else through this process's own pull.

use std::time::Duration;

use crate::acp::client::{DiscoveryError, HttpClient, HttpError, ManagerError};
use crate::containers::image_pull::PullState;
use crate::containers::ContainerRuntime;
use crate::daemon::DaemonEndpoint;
use crate::session::ContainerRuntimeName;

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
    ) -> PullReport {
        match self {
            Self::Daemon(client) => daemon(client, request).await,
            Self::Local => local(runtime, image.to_string(), request).await,
            Self::Unavailable => PullReport::Unavailable,
        }
    }
}

async fn daemon(client: &HttpClient, request: PullRequest) -> PullReport {
    let answer = match request {
        PullRequest::Poll => client.sandbox_image_status(false).await.map(|s| s.pull),
        PullRequest::Start => client.sandbox_image_pull().await,
        PullRequest::Cancel => client.sandbox_image_cancel().await,
    };
    match answer {
        Ok(pull) => PullReport::State(pull),
        Err(error) if is_refusal(&error) => match client.sandbox_image_status(false).await {
            Ok(status) => PullReport::Gated {
                command: status.pull_command,
            },
            Err(_) => PullReport::Unavailable,
        },
        Err(_) => PullReport::Unavailable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::image_pull::{self, PullKey};
    use crate::server::test_support;
    use std::time::Instant;

    const IMAGE: &str = "aoe-test/tui-pull-source:latest";

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
            PullReport::State(PullState::Idle)
        );
        let flight = stand_in();
        assert!(matches!(
            source.run(runtime, IMAGE, PullRequest::Start).await,
            PullReport::State(PullState::Running(_))
        ));
        assert_eq!(
            image_pull::waiters_for_test(&key()),
            2,
            "the stand-in and Start's"
        );
        assert_eq!(
            source.run(runtime, IMAGE, PullRequest::Cancel).await,
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
            PullReport::State(PullState::Idle)
        );
        for request in [PullRequest::Start, PullRequest::Cancel] {
            let PullReport::Gated { command } = source.run(runtime, IMAGE, request).await else {
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
