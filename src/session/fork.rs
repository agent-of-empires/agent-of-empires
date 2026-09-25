//! Pure fork-eligibility logic for terminal sessions, plus the one-shot fork seed a new session
//! carries.

use crate::agents::{get_agent, ForkStrategy};

/// The kind of one-shot fork a freshly-created session should perform on its first launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkSeed {
    /// Terminal fork: resume `parent_agent_session_id` with the agent's fork
    /// flag, writing to the pre-generated `child_session_id`.
    Terminal {
        parent: Box<crate::session::ConversationBinding>,
        child_session_id: String,
    },
    /// Structured fork: send ACP `session/fork` against
    /// `parent_acp_session_id`; the adapter mints the child id.
    Structured { parent_acp_session_id: String },
}

/// Why a fork was refused, for a user-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkDenied {
    /// The agent's CLI has no fork capability (terminal path).
    AgentCannotFork,
    /// No captured session id to fork from yet (the parent never started a
    /// conversation, or its id hasn't been observed).
    NoParentSession,
}

/// Process-wide default ACP registry, used only to answer "does this built-in tool have an ACP
/// adapter?" for capability checks that have no profile-resolved config handy.
fn builtin_acp_registry() -> &'static crate::acp::AgentRegistry {
    static REG: std::sync::OnceLock<crate::acp::AgentRegistry> = std::sync::OnceLock::new();
    REG.get_or_init(crate::acp::AgentRegistry::with_defaults)
}

/// True when `tool`/`agent_name` can run the structured ACP `session/fork` handshake: it maps to a
/// built-in ACP adapter AND that adapter is verified to implement ACP `session/fork`.
pub fn structured_fork_capable(tool: &str, agent_name: Option<&str>) -> bool {
    let resolved = agent_name.filter(|s| !s.is_empty()).unwrap_or(tool);
    builtin_acp_registry().get(resolved).is_some()
        && get_agent(resolved).is_some_and(|a| matches!(a.fork_strategy, ForkStrategy::ClaudeFork))
}

/// Whether a canonical native agent supports terminal forking.
pub fn terminal_agent_can_fork(agent: &str) -> bool {
    get_agent(agent).is_some_and(|a| !matches!(a.fork_strategy, ForkStrategy::Unsupported))
}

/// Decide whether a terminal session can be forked, and produce its one-shot seed.
pub fn terminal_fork_seed(
    parent: Option<&crate::session::ConversationBinding>,
    child_session_id: String,
) -> Result<ForkSeed, ForkDenied> {
    let parent = parent
        .filter(|parent| parent.is_known())
        .ok_or(ForkDenied::NoParentSession)?;
    let agent = parent
        .execution
        .as_ref()
        .and_then(|execution| get_agent(&execution.agent))
        .ok_or(ForkDenied::AgentCannotFork)?;
    if matches!(agent.fork_strategy, ForkStrategy::Unsupported) {
        return Err(ForkDenied::AgentCannotFork);
    }
    Ok(ForkSeed::Terminal {
        parent: Box::new(parent.clone()),
        child_session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};

    #[test]
    fn fork_requires_a_proven_parent_and_matching_native_capability() {
        let mut parent = ConversationBinding {
            session_id: "parent-uuid".into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/store".into()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: "/work".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        };
        assert!(matches!(
            terminal_fork_seed(Some(&parent), "child-uuid".into()),
            Ok(ForkSeed::Terminal { .. })
        ));
        for provenance in [
            ConversationProvenance::Unknown,
            ConversationProvenance::Preallocated,
        ] {
            parent.provenance = provenance;
            assert_eq!(
                terminal_fork_seed(Some(&parent), "child-uuid".into()),
                Err(ForkDenied::NoParentSession)
            );
        }
        parent.provenance = ConversationProvenance::Observed;
        parent.execution.as_mut().unwrap().agent = "gemini".into();
        assert_eq!(
            terminal_fork_seed(Some(&parent), "child-uuid".into()),
            Err(ForkDenied::AgentCannotFork)
        );
        assert_eq!(
            terminal_fork_seed(None, "child-uuid".into()),
            Err(ForkDenied::NoParentSession)
        );
    }
}
