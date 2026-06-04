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
pub mod client;
pub mod context_primer;
pub mod event_store;
pub mod fs_handler;
pub mod install_hints;
pub mod node;
pub mod permissions;
pub mod protocol;
pub mod runner;
#[cfg(feature = "serve")]
pub mod sandbox;
pub mod state;
pub mod supervisor;
pub mod terminal_handler;
pub mod worker_registry;

pub use agent_registry::{AgentRegistry, AgentSpec};
pub use approvals::{Approval, ApprovalDecision, Nonce};
pub use state::{AcpState, Event};
