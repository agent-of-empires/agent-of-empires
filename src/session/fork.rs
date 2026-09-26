//! Pure fork-eligibility logic for terminal sessions, plus the one-shot fork seed a new session
//! carries.

use crate::agents::{get_agent, ForkStrategy};
use crate::session::ConversationProvenance;

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
    /// No conversation id is recorded for the parent at all.
    NoParentSession,
    /// A conversation id is recorded, but nothing qualifies it, so the id
    /// cannot be shown to name a real conversation to fork. `None` when no
    /// binding stands behind it, so nothing says which conversation it names.
    UnqualifiedParent {
        provenance: Option<ConversationProvenance>,
    },
}

impl ForkDenied {
    /// The refusal phrased for a user. Every surface reads this, so a refusal
    /// cannot read three ways.
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::AgentCannotFork => "This agent has no native fork capability. Forkable agents: claude, codex, opencode.",
            Self::NoParentSession => "This session has no single captured conversation to fork from: it has captured none, or more than one session records this conversation id.",
            Self::UnqualifiedParent { provenance: None } => "This session records a conversation id that nothing qualifies: no record says which agent, store or directory it belongs to, so which conversation it names is unknown. Re-assert the id with 'aoe session set-session-id <session> <id>'.",
            Self::UnqualifiedParent { provenance: Some(ConversationProvenance::Preallocated) } => "This session has no captured conversation to fork from. Send it at least one message first.",
            Self::UnqualifiedParent { provenance: Some(_) } => "This session records a conversation id, but it was never qualified against a native agent. Run 'aoe session set-session-id <session> <id>' on it to qualify it.",
        }
    }
}

/// The conversation an explicit fork would carry, with the evidence for it
/// spelled out: a binding proves which native conversation the id names, a
/// bare recorded id proves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkParentRef<'a> {
    /// The id has a binding, whose provenance says how far that binding is trusted.
    Bound(&'a crate::session::ConversationBinding),
    /// The id is recorded but nothing qualifies it, so its origin cannot be
    /// named: a pre-pinned id that never ran, an id whose binding a degraded
    /// launch could not attest, and a live conversation seen outside a launch
    /// all look alike here.
    Recorded(&'a str),
}

impl<'a> ForkParentRef<'a> {
    /// The conversation id the parent records, however it was recorded.
    pub fn session_id(self) -> &'a str {
        match self {
            Self::Bound(binding) => &binding.session_id,
            Self::Recorded(session_id) => session_id,
        }
    }

    /// The binding, when the parent still has one.
    pub fn binding(self) -> Option<&'a crate::session::ConversationBinding> {
        match self {
            Self::Bound(binding) => Some(binding),
            Self::Recorded(_) => None,
        }
    }

    /// Whether the binding is strong enough to fork on.
    pub fn is_known(self) -> bool {
        self.binding()
            .is_some_and(crate::session::ConversationBinding::is_known)
    }
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
    parent: Option<ForkParentRef<'_>>,
    child_session_id: String,
) -> Result<ForkSeed, ForkDenied> {
    let parent = match parent {
        Some(ForkParentRef::Bound(parent)) if parent.is_known() => parent,
        Some(ForkParentRef::Bound(parent)) => {
            return Err(ForkDenied::UnqualifiedParent {
                provenance: Some(parent.provenance.clone()),
            })
        }
        Some(ForkParentRef::Recorded(_)) => {
            return Err(ForkDenied::UnqualifiedParent { provenance: None })
        }
        None => return Err(ForkDenied::NoParentSession),
    };
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
    use crate::session::{ConversationBinding, ExecutionBinding};

    fn bound(provenance: ConversationProvenance) -> ConversationBinding {
        ConversationBinding {
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
            provenance,
            transcript_path: None,
        }
    }

    /// A recorded id is refused as unqualified whatever the evidence, and the
    /// refusal keeps the evidence: a bound parent names its provenance, a
    /// binding-less one names none because none is left to read.
    #[test]
    fn fork_reports_a_recorded_but_unqualified_parent_separately() {
        for provenance in [
            ConversationProvenance::Unknown,
            ConversationProvenance::Preallocated,
        ] {
            let parent = bound(provenance.clone());
            assert_eq!(
                terminal_fork_seed(Some(ForkParentRef::Bound(&parent)), "child-uuid".into()),
                Err(ForkDenied::UnqualifiedParent {
                    provenance: Some(provenance)
                })
            );
        }
        assert_eq!(
            terminal_fork_seed(
                Some(ForkParentRef::Recorded("legacy-uuid")),
                "child-uuid".into()
            ),
            Err(ForkDenied::UnqualifiedParent { provenance: None })
        );
        assert_eq!(
            terminal_fork_seed(None, "child-uuid".into()),
            Err(ForkDenied::NoParentSession)
        );
    }

    #[test]
    fn fork_requires_a_qualified_parent_and_matching_native_capability() {
        let mut parent = bound(ConversationProvenance::Observed);
        assert!(matches!(
            terminal_fork_seed(Some(ForkParentRef::Bound(&parent)), "child-uuid".into()),
            Ok(ForkSeed::Terminal { .. })
        ));
        parent.execution.as_mut().unwrap().agent = "gemini".into();
        assert_eq!(
            terminal_fork_seed(Some(ForkParentRef::Bound(&parent)), "child-uuid".into()),
            Err(ForkDenied::AgentCannotFork)
        );
    }

    /// Every refusal state carries its own wording, and each names the remedy
    /// that state admits: a preallocated id has no conversation to qualify, and
    /// a binding-less one no provenance to assert.
    #[test]
    fn user_message_distinguishes_every_refusal_state() {
        let cases = [
            (
                ForkDenied::AgentCannotFork,
                "This agent has no native fork capability. Forkable agents: claude, codex, opencode.",
            ),
            (
                ForkDenied::NoParentSession,
                "This session has no single captured conversation to fork from: it has captured none, or more than one session records this conversation id.",
            ),
            (
                ForkDenied::UnqualifiedParent {
                    provenance: Some(ConversationProvenance::Preallocated),
                },
                "This session has no captured conversation to fork from. Send it at least one message first.",
            ),
            (
                ForkDenied::UnqualifiedParent {
                    provenance: Some(ConversationProvenance::Unknown),
                },
                "This session records a conversation id, but it was never qualified against a native agent. Run 'aoe session set-session-id <session> <id>' on it to qualify it.",
            ),
            (
                ForkDenied::UnqualifiedParent { provenance: None },
                "This session records a conversation id that nothing qualifies: no record says which agent, store or directory it belongs to, so which conversation it names is unknown. Re-assert the id with 'aoe session set-session-id <session> <id>'.",
            ),
        ];
        for (denied, expected) in cases {
            assert_eq!(denied.user_message(), expected);
        }
    }
}
