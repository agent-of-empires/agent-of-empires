//! Root façade for the Daemon client, local admission and runtime streams.

pub use aoe_daemon_client::*;

pub(crate) mod host_transport;
pub(crate) mod lifecycle;
pub mod live;
mod runtime;
mod runtime_connection;
pub(crate) mod websocket;

pub use host_transport::{local_unix_client, local_unix_endpoint};
pub use runtime::{CreationPhase, CreationProgress, RuntimeFrame, SandboxProgress};
pub(crate) use runtime::{NamespaceMutation, NamespaceOutcome};
pub use runtime_connection::{RuntimeConnection, RuntimeConnectionError, RuntimeEvent};
pub use websocket::WsError;

use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum LocalRuntimeError {
    #[error(transparent)]
    Client(#[from] DaemonClientError),
    #[error("Local daemon protocol {actual} does not match {expected}; run `aoe serve --restart`")]
    Protocol { expected: u16, actual: u16 },
    #[error(
        "Running daemon (PID {pid}) predates this build's local runtime transport. \
         Run `aoe serve --restart` with the new binary, or stop it through its manager"
    )]
    PredatesTransport { pid: u32 },
    #[error("Local daemon owner or namespace does not match")]
    Identity,
    #[error("Local daemon profile '{0}' is unavailable")]
    ProfileUnavailable(String),
}

/// How a reachable local Daemon's protocol differs from this client's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolSkew {
    pub(crate) daemon_older: bool,
    pub(crate) summary: String,
}

/// One process incarnation of the local Daemon; a PID alone can be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonIncarnation {
    pub(crate) pid: u32,
    pub(crate) birth: crate::process::ProcessBirthFingerprint,
}

impl DaemonIncarnation {
    pub(crate) fn of(pid: u32) -> anyhow::Result<Option<Self>> {
        Ok(
            crate::process::process_birth_identity(pid)?.map(|birth| Self {
                pid,
                birth: crate::process::process_birth_fingerprint(&birth),
            }),
        )
    }

    #[cfg(test)]
    pub(crate) fn sample(start_id: &str) -> Self {
        let birth = crate::process::ProcessBirthIdentity {
            boot_id: "boot".into(),
            start_id: start_id.into(),
        };
        Self {
            pid: 1,
            birth: crate::process::process_birth_fingerprint(&birth),
        }
    }
}

/// What can bring an older local Daemon back in step with this aoe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutdatedDaemonRemedy {
    /// This aoe may restart this incarnation in place.
    Restart(DaemonIncarnation),
    /// aoe launched it, but restarting is left to `aoe serve --restart`.
    Command,
    /// Only its own terminal or service manager can restart it.
    External,
}

/// A protocol skew, judged while the lifecycle transaction pinned the Daemon
/// that reported it. `remedy` is `None` for a Daemon newer than this aoe.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct PinnedProtocolSkew {
    pub(crate) message: String,
    pub(crate) summary: String,
    pub(crate) remedy: Option<OutdatedDaemonRemedy>,
}

impl LocalRuntimeError {
    pub(crate) fn protocol_skew(&self) -> Option<ProtocolSkew> {
        match *self {
            Self::Protocol { expected, actual } => Some(ProtocolSkew {
                daemon_older: actual < expected,
                summary: format!("Daemon protocol {actual}, this aoe {expected}"),
            }),
            Self::PredatesTransport { .. } => Some(ProtocolSkew {
                daemon_older: true,
                summary: "Daemon predates this aoe's local connection".to_owned(),
            }),
            Self::Client(_) | Self::Identity | Self::ProfileUnavailable(_) => None,
        }
    }
}

pub(crate) async fn local_runtime_ready(
    client: &DaemonClient,
    profile: Option<&str>,
) -> Result<RuntimeInfo, LocalRuntimeError> {
    let info = client.runtime_info().await?;
    if info.protocol_version != RUNTIME_PROTOCOL_VERSION {
        return Err(LocalRuntimeError::Protocol {
            expected: RUNTIME_PROTOCOL_VERSION,
            actual: info.protocol_version,
        });
    }
    if !matches_local_identity(client, &info) {
        return Err(LocalRuntimeError::Identity);
    }
    if info.health == RuntimeHealth::Healthy
        && (info.profiles.is_empty()
            || profile.is_some_and(|profile| !info.profiles.iter().any(|name| name == profile)))
    {
        return Err(LocalRuntimeError::ProfileUnavailable(
            profile.unwrap_or("default").into(),
        ));
    }
    Ok(info)
}

pub(crate) async fn local_daemon_ready(
    client: &DaemonClient,
    pid: u32,
) -> Result<bool, DaemonClientError> {
    let info = client.runtime_info_for_peer(pid).await?;
    Ok(info.protocol_version == RUNTIME_PROTOCOL_VERSION && matches_local_identity(client, &info))
}

fn matches_local_identity(client: &DaemonClient, info: &RuntimeInfo) -> bool {
    let namespace = client
        .unix_path()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::to_str);
    !info.epoch.is_empty()
        && info.local_owner
        && namespace.is_some_and(|expected| expected == info.namespace)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod suggestions_tests;
