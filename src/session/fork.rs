//! Pure fork-eligibility logic for terminal sessions, plus the one-shot fork seed a new session
//! carries.

use crate::agents::{get_agent, ForkStrategy};

/// The kind of one-shot fork a freshly-created session should perform on its first launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkSeed {
    /// Terminal fork: resume `parent_agent_session_id` with the agent's fork
    /// flag, writing to the pre-generated `child_session_id`.
    Terminal {
        parent_agent_session_id: String,
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

/// True when `tool` is a terminal agent whose CLI can fork (claude/codex/ opencode).
pub fn terminal_agent_can_fork(tool: &str) -> bool {
    get_agent(tool).is_some_and(|a| !matches!(a.fork_strategy, ForkStrategy::Unsupported))
}

/// Decide whether a TERMINAL session can be forked, and if so produce the seed.
pub fn terminal_fork_seed(
    tool: &str,
    parent_agent_session_id: Option<&str>,
    child_session_id: String,
) -> Result<ForkSeed, ForkDenied> {
    let agent = get_agent(tool).ok_or(ForkDenied::AgentCannotFork)?;
    if matches!(agent.fork_strategy, ForkStrategy::Unsupported) {
        return Err(ForkDenied::AgentCannotFork);
    }
    let parent = parent_agent_session_id
        .filter(|s| !s.is_empty())
        .ok_or(ForkDenied::NoParentSession)?;
    Ok(ForkSeed::Terminal {
        parent_agent_session_id: parent.to_string(),
        child_session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_with_parent_id_yields_terminal_seed() {
        let seed = terminal_fork_seed("claude", Some("parent-uuid"), "child-uuid".into());
        assert_eq!(
            seed,
            Ok(ForkSeed::Terminal {
                parent_agent_session_id: "parent-uuid".into(),
                child_session_id: "child-uuid".into(),
            })
        );
    }

    #[test]
    fn resume_only_agent_is_denied() {
        assert_eq!(
            terminal_fork_seed("gemini", Some("parent-uuid"), "child-uuid".into()),
            Err(ForkDenied::AgentCannotFork)
        );
    }

    #[test]
    fn missing_parent_id_is_denied() {
        assert_eq!(
            terminal_fork_seed("claude", None, "child-uuid".into()),
            Err(ForkDenied::NoParentSession)
        );
        assert_eq!(
            terminal_fork_seed("claude", Some(""), "child-uuid".into()),
            Err(ForkDenied::NoParentSession)
        );
    }
}
