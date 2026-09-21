//! Acp: native rendering of structured agent state via ACP.

pub mod acp_client;
pub mod adapters;
pub mod agent_compat;
pub mod agent_policy;
pub mod agent_profiles;
pub mod agent_registry;
pub mod approvals;
pub mod background_agent;
pub mod capability_probe;
pub mod client;
pub mod context_primer;
pub mod control_cache;
pub mod control_protocol;
pub mod dispatch;
pub mod elicitations;
pub mod event_store;
pub mod fs_handler;
pub mod install_hints;
pub mod mcp_config;
pub mod node;
/// Recall cache of per-agent ACP config options, consumed by the dashboard's
/// defaults page.
pub mod option_catalog;
pub mod permissions;
pub mod protocol;
pub mod runner_lifecycle;
pub mod sandbox;
pub mod session_paths;
pub mod session_tee;
pub mod state;
pub mod supervisor;
pub mod terminal_handler;
pub mod transcript;
pub mod version_probe;

pub use agent_registry::{
    inherited_acp_base, pick_acp_agent_name, pinned_model_for_tool, AgentRegistry, AgentSpec,
};
pub use approvals::{Approval, ApprovalDecision, Nonce};
pub use state::{AcpState, Event};
