//! Cockpit daemon client.
//!
//! HTTP + WebSocket client for talking to an `aoe serve` daemon. Used
//! by:
//!
//! - The `aoe cockpit *` CLI verbs (history, status, prompt, approve,
//!   cancel, tail, attach).
//! - The TUI cockpit view (`src/tui/cockpit_view/`).
//!
//! All three layers share `DaemonEndpoint` discovery, the typed
//! `HttpClient`, and the typed `WsHandle` so a change to the wire
//! shape breaks every consumer at compile time, not at runtime.
//!
//! Discovery resolution order:
//!
//! 1. `AOE_DAEMON_URL` (+ optional `AOE_DAEMON_TOKEN`).
//! 2. Local `<app_dir>/serve.url` paired with a live `serve.pid`.
//!
//! [`daemon_manager::require_daemon`] returns
//! [`daemon_manager::ManagerError::NoDaemonRunning`] when neither
//! resolves; callers render the contained hint and bail rather than
//! starting a daemon by side-effect, so the user keeps the choice
//! between localhost, Tailscale, and Cloudflare explicit.

pub mod daemon_manager;
pub mod discovery;
pub mod http;
pub mod ws;

pub use daemon_manager::{require_daemon, ManagerError};
pub use discovery::{discover, DaemonEndpoint, DiscoveryError, Source};
pub use http::{HttpClient, HttpError, REPLAY_PAGE_SIZE};
pub use ws::{connect as ws_connect, WsError, WsHandle, WsMessage};
