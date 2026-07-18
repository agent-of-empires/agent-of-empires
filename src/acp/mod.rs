//! Acp: native rendering of structured agent state via ACP.
//!
//! Architecture summary (see design doc v4 for the full picture):
//!
//! - aoe is an ACP **client**.
//! - Backends are ACP **agents** spawned as subprocesses.
//! - Day-one backends: `claude-code` (Anthropic's official ACP adapter) and
//!   `aoe-agent` (our Node binary, Vercel AI SDK 6).
//! - File-system access (`fs/*`) and terminal execution (`terminal/*`) are
//!   delegated from the agent to aoe via ACP. aoe owns the disk; the agent
//!   only orchestrates the model.
//! - State lives behind a single-writer actor; all mutations flow through
//!   `state::apply_event`.

pub mod acp_client;
#[cfg(feature = "serve")]
pub mod agent_compat;
pub mod agent_profiles;
pub mod agent_registry;
pub mod approvals;
pub mod background_agent;
pub mod client;
pub mod context_primer;
pub mod elicitations;
pub mod event_store;
pub mod fs_handler;
pub mod install_hints;
pub mod mcp_config;
pub mod node;
/// Recall cache of per-agent ACP config options; consumed only by the web
/// dashboard defaults page, so it is compiled with the serve feature.
#[cfg(feature = "serve")]
pub mod option_catalog;
pub mod permissions;
pub mod protocol;
#[cfg(feature = "serve")]
pub mod sandbox;
pub mod session_paths;
pub mod session_tee;
pub mod state;
pub mod supervisor;
pub mod terminal_handler;
#[cfg(feature = "serve")]
pub mod version_probe;

pub use agent_registry::{AgentRegistry, AgentSpec};
pub use approvals::{Approval, ApprovalDecision, Nonce};
pub use state::{AcpState, Event};
