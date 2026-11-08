//! Serve observations and guarded control operations, never run by the event loop.

use tokio_util::sync::CancellationToken;

use super::daemon::*;
use super::jobs::Control;
use super::{RestartProbe, ServeMode, ServeViewState, TransportStatus, TunnelTransport};
use crate::cli::serve::{TuiControlLease, TuiControlTarget, TuiObservationLease};

pub(super) enum Job {
    Open,
    Passphrase,
    ErrorContext,
    Picker,
    Providers,
    Retry(TuiControlTarget),
    Reset(TuiControlTarget),
    Restart(ServeMode, Option<TunnelTransport>),
    LaunchContext {
        needs_passphrase: bool,
    },
    LaunchOutcome {
        target: TuiControlTarget,
        mode: ServeMode,
        transport: Option<TunnelTransport>,
        passphrase: Option<String>,
        log_offset: u64,
    },
    ObserveStarting(TuiControlTarget),
    ObserveActive,
    Copy(&'static str, String),
}

pub(super) struct Snapshot {
    pub(super) state: ServeViewState,
    pub(super) target: TuiControlTarget,
    pub(super) passphrase: Option<String>,
    pub(super) local_primary: Option<String>,
}

pub(super) struct StartingObservation {
    pub(super) running: bool,
    pub(super) target: TuiControlTarget,
    pub(super) urls: Vec<crate::cli::serve::ServeUrl>,
    pub(super) passphrase: Option<String>,
    pub(super) log_offset: u64,
    pub(super) tail: Vec<String>,
}

pub(super) enum JobResult {
    Snapshot(Snapshot),
    Passphrase(String),
    ErrorContext(TuiControlTarget),
    Providers(TransportStatus, TransportStatus),
    Retried {
        tailscale: TransportStatus,
        cloudflare: TransportStatus,
        target: TuiControlTarget,
    },
    Reset(Result<(), String>),
    Restart(RestartProbe),
    LaunchContext {
        persists: bool,
        log_offset: u64,
        passphrase: Option<String>,
    },
    Starting(StartingObservation),
    ActiveLog(u64),
    Copied(&'static str),
    Failed(String),
}

fn snapshot(
    state: ServeViewState,
    passphrase: Option<String>,
    local_primary: Option<String>,
) -> JobResult {
    match crate::cli::serve::tui_control_target() {
        Ok(target) => JobResult::Snapshot(Snapshot {
            state,
            target,
            passphrase,
            local_primary,
        }),
        Err(error) => JobResult::Failed(error.to_string()),
    }
}

fn picker(cancel: &CancellationToken, passphrase: Option<String>) -> JobResult {
    let (state, primary) = ServeViewState::mode_picker(cancel);
    snapshot(state, passphrase, Some(primary))
}

fn observe(
    mut lease: TuiObservationLease,
    read_passphrase: bool,
) -> anyhow::Result<StartingObservation> {
    let mut urls = if lease.has_dashboard() {
        crate::cli::serve::read_serve_urls()
    } else {
        Vec::new()
    };
    let mut passphrase = (read_passphrase && lease.has_passphrase() && !urls.is_empty())
        .then(current_passphrase)
        .flatten();
    let log_offset = log_file_size();
    lease.recheck()?;
    let running = lease.is_running();
    let tail = if running {
        Vec::new()
    } else {
        urls.clear();
        passphrase = None;
        lease.startup_tail()
    };
    Ok(StartingObservation {
        running,
        urls,
        passphrase,
        log_offset,
        tail,
        target: lease.into_target(),
    })
}

pub(super) fn run_job(job: Job, control: &Control, runtime: &tokio::runtime::Runtime) -> JobResult {
    let cancel = &control.cancel;
    match job {
        Job::Open => runtime
            .block_on(async {
                let passphrase = load_or_generate_passphrase();
                let lease = TuiObservationLease::prepare(None, cancel).await?;
                if !lease.has_dashboard() {
                    drop(lease);
                    return Ok::<_, anyhow::Error>(picker(cancel, Some(passphrase)));
                }
                let mode = read_serve_mode().unwrap_or(ServeMode::Tunnel);
                let observation = observe(lease, mode == ServeMode::Tunnel)?;
                let state = if observation.running {
                    ServeViewState::running(
                        mode,
                        None,
                        observation.passphrase,
                        observation.urls,
                        observation.log_offset,
                    )
                } else {
                    ServeViewState::start_failed_with(
                        mode,
                        None,
                        "The observed Daemon exited before access metadata was read.",
                        observation.tail,
                    )
                };
                Ok(JobResult::Snapshot(Snapshot {
                    state,
                    target: observation.target,
                    passphrase: Some(passphrase),
                    local_primary: None,
                }))
            })
            .unwrap_or_else(|error| JobResult::Failed(error.to_string())),
        Job::Passphrase => JobResult::Passphrase(load_or_generate_passphrase()),
        Job::ErrorContext => match crate::cli::serve::tui_control_target() {
            Ok(target) => JobResult::ErrorContext(target),
            Err(error) => JobResult::Failed(error.to_string()),
        },
        Job::Picker => picker(cancel, None),
        Job::Providers => {
            let (tailscale, cloudflare) = assess_transports(cancel);
            JobResult::Providers(tailscale, cloudflare)
        }
        Job::Retry(target) => {
            let stopped = runtime.block_on(async {
                let lease = TuiControlLease::prepare(target, cancel).await?;
                anyhow::ensure!(control.admit(), "Serve retry cancelled before admission");
                lease.stop().await?;
                crate::cli::serve::tui_control_target()
            });
            control.finish_mutation();
            match stopped {
                Ok(target) => {
                    let (tailscale, cloudflare) = assess_transports(cancel);
                    JobResult::Retried {
                        tailscale,
                        cloudflare,
                        target,
                    }
                }
                Err(error) => {
                    tracing::warn!(target: "tui.dialog", %error, "Serve retry stop did not complete");
                    JobResult::Failed(format!(
                        "Stop did not complete: {error}. Inspect `aoe serve --status` before retrying."
                    ))
                }
            }
        }
        Job::Reset(target) => {
            let reset = runtime.block_on(async {
                let _lease = TuiControlLease::prepare(target, cancel).await?;
                anyhow::ensure!(control.admit(), "Funnel reset cancelled before admission");
                run_tailscale_funnel_reset(cancel).map_err(anyhow::Error::msg)
            });
            if let Err(error) = &reset {
                tracing::warn!(target: "tui.dialog", %error, "Funnel reset did not complete");
            }
            JobResult::Reset(reset.map_err(|error| error.to_string()))
        }
        Job::Restart(mode, transport) => {
            JobResult::Restart(RestartProbe::now(mode, transport, cancel))
        }
        Job::LaunchContext { needs_passphrase } => JobResult::LaunchContext {
            persists: super::persist_sessions(),
            log_offset: log_file_size(),
            passphrase: needs_passphrase.then(load_or_generate_passphrase),
        },
        Job::LaunchOutcome {
            target,
            mode,
            transport,
            passphrase,
            log_offset,
        } => runtime
            .block_on(async {
                let lease = TuiObservationLease::prepare(Some(target), cancel).await?;
                let observation =
                    observe(lease, passphrase.is_none() && mode == ServeMode::Tunnel)?;
                if !observation.running {
                    return Ok::<_, anyhow::Error>(JobResult::Starting(observation));
                }
                let state = ServeViewState::running(
                    mode,
                    transport,
                    passphrase.or(observation.passphrase),
                    observation.urls,
                    log_offset,
                );
                Ok(JobResult::Snapshot(Snapshot {
                    state,
                    target: observation.target,
                    passphrase: None,
                    local_primary: None,
                }))
            })
            .unwrap_or_else(|error| JobResult::Failed(error.to_string())),
        Job::ObserveStarting(target) => runtime
            .block_on(async {
                let lease = TuiObservationLease::prepare(Some(target), cancel).await?;
                observe(lease, true).map(JobResult::Starting)
            })
            .unwrap_or_else(|error| JobResult::Failed(error.to_string())),
        Job::ObserveActive => JobResult::ActiveLog(log_file_size()),
        Job::Copy(label, value) => {
            if !control.admit() {
                return JobResult::Failed("Clipboard copy cancelled before admission".to_owned());
            }
            crate::tui::clipboard::copy_to_clipboard(&value);
            JobResult::Copied(label)
        }
    }
}
