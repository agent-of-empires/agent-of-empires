//! Acp daemon client.
//!
//! HTTP + WebSocket client for talking to an `aoe serve` daemon. Used
//! by:
//!
//! - The `aoe acp *` CLI verbs (history, status, prompt, approve,
//!   cancel, tail, attach).
//! - The TUI structured view (`src/tui/structured_view/`).
//!
//! Clients share endpoint discovery, bounded HTTP decoding and authenticated
//! WebSocket transport. An explicit `AOE_DAEMON_URL` takes precedence; otherwise
//! discovery selects the live local daemon through its owner-verified Unix socket.
//! [`daemon_manager::require_daemon`] never starts a process. The explicit
//! [`daemon_manager::ensure_local_daemon`] bootstrap uses the serialized localhost launcher
//! and never replaces an explicit remote endpoint with a local daemon.

pub mod daemon_manager;
pub mod discovery;
pub mod http;
pub mod ws;

pub use daemon_manager::{require_daemon, require_local_daemon, ManagerError};
pub use discovery::{discover, DaemonEndpoint, DiscoveryError, Source};
pub use http::{HttpClient, HttpError, PluginCommandView, REPLAY_PAGE_SIZE};
pub use ws::{connect as ws_connect, connect_with as ws_connect_with, WsHandle, WsMessage};
