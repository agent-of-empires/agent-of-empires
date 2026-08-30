//! Web dashboard for remote agent session access
//!
//! Provides an embedded axum web server that serves a responsive dashboard
//! for monitoring and interacting with agent sessions from any browser.

pub(crate) mod access;
pub(crate) mod acp_events;
#[cfg(feature = "serve")]
pub mod acp_reconciler;
#[cfg(feature = "serve")]
pub mod acp_ws;
pub mod api;
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
#[cfg(all(feature = "serve", any(test, feature = "test-support")))]
#[doc(hidden)]
pub mod test_support;
pub(crate) mod token;
pub mod tunnel;

/// Re-export of the broadcast frame defined in `crate::acp::protocol`,
/// kept under `crate::server::` so existing supervisor/WS call sites keep
/// resolving without churn. The canonical definition lives in protocol.rs
/// so the daemon and any client share a single source of truth.
#[cfg(feature = "serve")]
pub use crate::acp::protocol::AcpBroadcastFrame;
pub(crate) use access::{is_untrusted_ip_literal, is_wildcard_bind, norm_host};
pub(crate) use acp_events::{apply_status_intent, derive_acp_status};
pub use assets::web_build_id;
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
