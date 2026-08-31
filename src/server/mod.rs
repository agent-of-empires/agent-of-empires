//! The daemon: an axum server exposing the REST/WS API that the dashboard,
//! the TUI structured view, and ACP clients all speak. The embedded dashboard
//! bundle it can also serve is optional; the `web` feature gates it, and
//! `assets` holds the serving code.

pub(crate) mod access;
pub(crate) mod acp_events;
pub mod acp_reconciler;
pub mod acp_ws;
pub mod api;
#[cfg(feature = "web")]
pub(crate) mod assets;
pub(crate) mod attach_project;
pub mod auth;
pub mod callback;
pub(crate) mod disk_watch;
pub(crate) mod idle_reap;
pub(crate) mod ip_discovery;
pub mod live_ws;
pub mod login;
mod pane;
pub mod push;
pub mod push_send;
pub mod rate_limit;
pub(crate) mod reload;
pub(crate) mod router;
pub(crate) mod serve_snapshot;
pub(crate) mod session_identity;
pub(crate) mod session_service;
pub(crate) mod session_spawn;
pub(crate) mod sleep_inhibit;
pub(crate) mod startup;
pub(crate) mod startup_recovery;
pub(crate) mod state;
pub(crate) mod status_poll;
pub(crate) mod structured_repair;
#[cfg(test)]
mod test_helpers;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
pub(crate) mod token;
pub mod tunnel;

/// Re-export of the broadcast frame defined in `crate::acp::protocol`,
/// kept under `crate::server::` so existing supervisor/WS call sites keep
/// resolving without churn. The canonical definition lives in protocol.rs
/// so the daemon and any client share a single source of truth.
pub use crate::acp::protocol::AcpBroadcastFrame;
pub(crate) use access::{is_untrusted_ip_literal, is_wildcard_bind, norm_host};
pub(crate) use acp_events::{apply_status_intent, derive_acp_status};
#[cfg(feature = "web")]
pub use assets::web_build_id;

/// No dashboard is embedded, so there is no bundle identity to report.
#[cfg(not(feature = "web"))]
pub fn web_build_id() -> Option<&'static str> {
    None
}
pub(crate) use disk_watch::{
    add_profile_disk_watch, remove_profile_disk_watch, rename_profile_disk_watch,
};
pub use ip_discovery::{discover_tagged_ips, IpKind};
pub use serve_snapshot::{FormFactorCounters, StructuredTelemetryCounters};
pub(crate) use sleep_inhibit::{
    SLEEP_INHIBIT_SNAPSHOT_ENABLED, SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT,
};
pub(crate) use startup::resolve_auth_mode;
pub use startup::{start_server, ServerConfig};
pub use state::{AppState, CleanupDefaultsCache};
pub(crate) use token::generate_token;
pub use token::TokenManager;
