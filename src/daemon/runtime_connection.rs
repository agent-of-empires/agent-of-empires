//! One native runtime stream, with connection-scoped identity and revocable grants.

use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio_tungstenite::tungstenite::Message;

use super::{
    websocket::{self, NativeSocket},
    CreationProgress, RuntimeFrame, RuntimeInfo, RuntimeSnapshot, WsError,
    RUNTIME_PROTOCOL_VERSION,
};
use crate::acp::client::DaemonEndpoint;

#[derive(Debug, Error)]
pub enum RuntimeConnectionError {
    #[error(transparent)]
    WebSocket(#[from] WsError),
    #[error("daemon runtime protocol is incompatible")]
    Protocol,
    #[error("daemon runtime identity does not match the selected endpoint")]
    Identity,
    #[error("requested profile is not available on this daemon")]
    ProfileUnavailable,
}

pub struct RuntimeConnection {
    socket: NativeSocket,
    info: RuntimeInfo,
    snapshot: Arc<RuntimeSnapshot>,
    creation_progress: Vec<CreationProgress>,
    local_equivalent: bool,
    connected: bool,
}

/// One item from the runtime stream. Canonical state arrives as snapshots;
/// advisory creation progress arrives without any canonical change.
#[derive(Clone)]
pub enum RuntimeEvent {
    Snapshot(Arc<RuntimeSnapshot>),
    Progress(Vec<CreationProgress>),
}

impl RuntimeConnection {
    pub async fn connect(
        endpoint: &DaemonEndpoint,
        requested_profile: Option<&str>,
    ) -> Result<Self, RuntimeConnectionError> {
        let mut socket = websocket::connect(endpoint, "/api/runtime/ws", None).await?;
        let (info, snapshot) = tokio::time::timeout(Duration::from_secs(15), async {
            let RuntimeFrame::Hello(info) = receive(&mut socket).await? else {
                return Err(RuntimeConnectionError::Protocol);
            };
            if info.protocol_version != RUNTIME_PROTOCOL_VERSION || info.epoch.is_empty() {
                return Err(RuntimeConnectionError::Protocol);
            }
            let RuntimeFrame::Snapshot(snapshot) = receive(&mut socket).await? else {
                return Err(RuntimeConnectionError::Protocol);
            };
            Ok((info, snapshot))
        })
        .await
        .map_err(|_| WsError::Daemon(super::DaemonClientError::Timeout))??;
        if snapshot.cursor.epoch != info.epoch || snapshot.cursor.revision == 0 {
            return Err(RuntimeConnectionError::Protocol);
        }
        if info.local_owner && endpoint.unix_path().is_none() {
            return Err(RuntimeConnectionError::Identity);
        }
        let selected = requested_profile.unwrap_or(&snapshot.contents.default_profile);
        if !snapshot
            .contents
            .profiles
            .iter()
            .any(|profile| profile.name == selected)
        {
            return Err(RuntimeConnectionError::ProfileUnavailable);
        }
        let local_equivalent = if let Some(path) = endpoint.unix_path() {
            let namespace = path
                .parent()
                .and_then(std::path::Path::parent)
                .and_then(std::path::Path::to_str);
            if !info.local_owner || !namespace.is_some_and(|namespace| namespace == info.namespace)
            {
                return Err(RuntimeConnectionError::Identity);
            }
            if info.read_only || info.cityhall_mode {
                false
            } else {
                let locator = tokio::task::spawn_blocking(crate::tmux::native_socket_locator)
                    .await
                    .map_err(|_| RuntimeConnectionError::Identity)?;
                locator
                    .as_deref()
                    .and_then(std::path::Path::to_str)
                    .zip(info.tmux_socket.as_deref())
                    .is_some_and(|(local, remote)| local == remote)
            }
        } else {
            false
        };
        Ok(Self {
            socket,
            info,
            snapshot: Arc::new(snapshot),
            creation_progress: Vec::new(),
            local_equivalent,
            connected: true,
        })
    }

    /// Handshake metadata; current health and grants come from the latest snapshot.
    pub fn info(&self) -> &RuntimeInfo {
        &self.info
    }

    pub fn snapshot(&self) -> &Arc<RuntimeSnapshot> {
        &self.snapshot
    }

    /// Latest creation progress; empty on streams without the local-owner grant.
    pub fn creation_progress(&self) -> &[CreationProgress] {
        &self.creation_progress
    }

    pub fn mutations_allowed(&self) -> bool {
        self.connected
            && !self.info.read_only
            && self.snapshot.contents.health == super::RuntimeHealth::Healthy
            && self.snapshot.contents.capabilities.mutations
    }

    pub fn native_interaction_allowed(&self) -> bool {
        self.mutations_allowed()
            && self.local_equivalent
            && self.snapshot.contents.capabilities.native_interaction
    }

    /// Next stream item, including progress frames that carry no canonical
    /// change. Use this when creation progress matters; `next_snapshot` skips
    /// them and would block until the next canonical revision.
    pub async fn next_event(&mut self) -> Result<RuntimeEvent, RuntimeConnectionError> {
        if !self.connected {
            return Err(WsError::UnexpectedClose(None).into());
        }
        let result = self.receive_event().await;
        if result.is_err() {
            self.connected = false;
        }
        result
    }

    pub async fn next_snapshot(&mut self) -> Result<Arc<RuntimeSnapshot>, RuntimeConnectionError> {
        loop {
            match self.next_event().await? {
                RuntimeEvent::Snapshot(snapshot) => return Ok(snapshot),
                RuntimeEvent::Progress(_) => {}
            }
        }
    }

    async fn receive_event(&mut self) -> Result<RuntimeEvent, RuntimeConnectionError> {
        loop {
            match receive(&mut self.socket).await? {
                RuntimeFrame::Snapshot(snapshot) => {
                    if snapshot.cursor.epoch != self.info.epoch || snapshot.cursor.revision == 0 {
                        return Err(RuntimeConnectionError::Protocol);
                    }
                    if snapshot.cursor.revision <= self.snapshot.cursor.revision {
                        continue;
                    }
                    self.snapshot = Arc::new(snapshot);
                    return Ok(RuntimeEvent::Snapshot(self.snapshot.clone()));
                }
                // Advisory and idempotent: cache it so `creation_progress`
                // reflects the newest state even for a snapshot-only caller.
                RuntimeFrame::Creation(progress) => {
                    self.creation_progress = progress;
                    return Ok(RuntimeEvent::Progress(self.creation_progress.clone()));
                }
                RuntimeFrame::Hello(_) => return Err(RuntimeConnectionError::Protocol),
            }
        }
    }

    pub async fn close(mut self) {
        self.connected = false;
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            send(&mut self.socket, Message::Close(None)),
        )
        .await;
    }
}

async fn send(socket: &mut NativeSocket, message: Message) -> Result<(), WsError> {
    tokio::time::timeout(Duration::from_secs(15), async {
        match socket {
            NativeSocket::Unix(stream) => stream.send(message).await?,
            NativeSocket::Tcp(stream) => stream.send(message).await?,
        }
        Ok(())
    })
    .await
    .map_err(|_| WsError::Daemon(super::DaemonClientError::Timeout))?
}

async fn receive(socket: &mut NativeSocket) -> Result<RuntimeFrame, RuntimeConnectionError> {
    loop {
        let incoming = tokio::time::timeout(Duration::from_secs(90), async {
            match socket {
                NativeSocket::Unix(stream) => stream.next().await,
                NativeSocket::Tcp(stream) => stream.next().await,
            }
        })
        .await
        .map_err(|_| WsError::Daemon(super::DaemonClientError::Timeout))?;
        match incoming {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|_| WsError::Parse.into())
            }
            Some(Ok(Message::Ping(payload))) => send(socket, Message::Pong(payload)).await?,
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                return Err(WsError::UnexpectedClose(frame.map(|frame| frame.code)).into())
            }
            Some(Ok(_)) => return Err(RuntimeConnectionError::Protocol),
            Some(Err(error)) => return Err(WsError::from(error).into()),
            None => return Err(WsError::UnexpectedClose(None).into()),
        }
    }
}
