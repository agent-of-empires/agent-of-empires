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
    AgentCannotFork { agent: String },
    /// The parent names no single conversation to fork: no row carries the id,
    /// or the rows that do name different conversations.
    NoParentSession,
    /// A conversation id is recorded, but no qualified record names the
    /// conversation it is, so it cannot be shown to name a conversation to fork.
    /// `pre_pinned` is true only when a binding proves the id was reserved by a
    /// launch that never ran: such an id names no conversation yet, so the
    /// remedy is a message rather than an assertion about a conversation that
    /// does not exist. `recorded` is the id the parent carries, and it is the
    /// value a qualification re-asserts.
    UnqualifiedParent { pre_pinned: bool, recorded: String },
    /// The parent is a fork whose launch has not happened, so it holds no
    /// conversation of its own: the first launch is the fork.
    UnlaunchedFork,
}

impl ForkDenied {
    /// The refusal phrased for a user, so the wording and its remedy live in
    /// one place. `title` names the session as the user knows it; `id` is what
    /// the remedy carries, because `resolve_session` resolves a title to
    /// whichever row it meets first, so an id is the only session name a
    /// printed command can act on.
    pub fn user_message(&self, title: &str, id: &str) -> String {
        match self {
            Self::AgentCannotFork { agent } => format!(
                "Nothing to fork: session '{title}' runs agent '{agent}', which has no native fork capability. Forkable agents: claude, codex, opencode."
            ),
            Self::NoParentSession => format!(
                "Nothing to fork: session '{title}' has no single captured conversation to fork from: it has captured none, or more than one session records this conversation id."
            ),
            Self::UnqualifiedParent { pre_pinned: false, recorded } => format!(
                "Nothing to fork: session '{title}' records conversation '{recorded}', which nothing qualifies, so which conversation it names is unknown. Qualify it with `{command}`, and for a pi or omp session add a --store flag naming the absolute path of its transcript.",
                command = qualify_command(id, recorded)
            ),
            Self::UnqualifiedParent { pre_pinned: true, .. } => format!(
                "Nothing to fork: session '{title}' has no captured conversation to fork from. Send it at least one message first."
            ),
            Self::UnlaunchedFork => format!(
                "Nothing to fork: session '{title}' is a fork that has not launched yet. Start it once, then fork from the child conversation."
            ),
        }
    }
}

/// The one command that re-asserts a recorded id. It carries the session id,
/// not the title, because `resolve_session` resolves a title to whichever row
/// it meets first, and each value is quoted so one holding a space stays one
/// argument. Backticks delimit it in prose: the apostrophe would be one more
/// shell metacharacter to demangle, the backtick only marks a span.
fn qualify_command(id: &str, recorded: &str) -> String {
    format!(
        "aoe session set-session-id {} {}",
        shell_words::quote(id),
        shell_words::quote(recorded)
    )
}

/// The conversation an explicit fork would carry, with the evidence for it
/// spelled out: a binding proves which native conversation the id names, a
/// bare recorded id proves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkParentRef<'a> {
    /// The id has a binding, whose provenance says how far that binding is
    /// trusted, so an unqualified one is still evidence of something.
    Bound(&'a crate::session::ConversationBinding),
    /// The id is recorded with no binding behind it, so no record names the
    /// agent, store or directory it belongs to: a degraded launch dropped the
    /// binding it could not attest, or the row predates bindings.
    Recorded(&'a str),
    /// The row is a fork whose launch has not happened, so it holds no
    /// conversation of its own.
    Unlaunched,
}

impl<'a> ForkParentRef<'a> {
    /// The conversation the parent records, or `None` when the row holds none
    /// of its own, so it names no conversation a fork could name.
    pub fn session_id(self) -> Option<&'a str> {
        match self {
            Self::Bound(binding) => Some(&binding.session_id),
            Self::Recorded(session_id) => Some(session_id),
            Self::Unlaunched => None,
        }
    }

    /// The binding, when the parent still has one.
    pub fn binding(self) -> Option<&'a crate::session::ConversationBinding> {
        match self {
            Self::Bound(binding) => Some(binding),
            Self::Recorded(_) | Self::Unlaunched => None,
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
    // One refusal for every id nothing qualifies: only a binding can prove the
    // id was reserved by a launch that never ran, and a row with no binding
    // proves nothing at all.
    let unqualified = |binding: Option<&crate::session::ConversationBinding>, recorded: &str| {
        ForkDenied::UnqualifiedParent {
            pre_pinned: binding.is_some_and(|binding| {
                matches!(binding.provenance, ConversationProvenance::Preallocated)
            }),
            recorded: recorded.to_string(),
        }
    };
    // A qualified binding is the whole gate, and it is stricter than the
    // resume path, which also accepts a binding migration left unattributed.
    let parent = match parent {
        Some(ForkParentRef::Bound(parent)) if parent.is_known() => parent,
        Some(ForkParentRef::Bound(parent)) => {
            return Err(unqualified(Some(parent), &parent.session_id))
        }
        Some(ForkParentRef::Recorded(recorded)) => return Err(unqualified(None, recorded)),
        Some(ForkParentRef::Unlaunched) => return Err(ForkDenied::UnlaunchedFork),
        None => return Err(ForkDenied::NoParentSession),
    };
    // `is_known` accepted the binding only on the strength of its execution, so
    // a missing one is an inconsistency nothing can qualify.
    let Some(execution) = parent.execution.as_ref() else {
        return Err(unqualified(None, &parent.session_id));
    };
    let forkable = get_agent(&execution.agent)
        .is_some_and(|agent| !matches!(agent.fork_strategy, ForkStrategy::Unsupported));
    if !forkable {
        return Err(ForkDenied::AgentCannotFork {
            agent: execution.agent.clone(),
        });
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
                cwd: "/work".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance,
            transcript_path: None,
        }
    }

    /// A recorded id is refused as unqualified whatever the evidence behind it,
    /// and only a pre-pinned one is told to send a message, while a fork whose
    /// launch has not happened is refused as itself.
    #[test]
    fn fork_reports_a_recorded_but_unqualified_parent_separately() {
        for (provenance, pre_pinned) in [
            (ConversationProvenance::Preallocated, true),
            (ConversationProvenance::Unknown, false),
        ] {
            let parent = bound(provenance.clone());
            assert_eq!(
                terminal_fork_seed(Some(ForkParentRef::Bound(&parent)), "child-uuid".into()),
                Err(ForkDenied::UnqualifiedParent {
                    pre_pinned,
                    recorded: "parent-uuid".into(),
                }),
                "{provenance:?}"
            );
        }
        // A binding nothing qualified and no binding at all are one refusal:
        // neither proves the id was reserved by a launch that never ran.
        assert_eq!(
            terminal_fork_seed(
                Some(ForkParentRef::Recorded("legacy-uuid")),
                "child-uuid".into()
            ),
            Err(ForkDenied::UnqualifiedParent {
                pre_pinned: false,
                recorded: "legacy-uuid".into(),
            })
        );
        assert_eq!(
            terminal_fork_seed(Some(ForkParentRef::Unlaunched), "child-uuid".into()),
            Err(ForkDenied::UnlaunchedFork)
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
            Err(ForkDenied::AgentCannotFork {
                agent: "gemini".into()
            })
        );
    }

    /// Every refusal state carries its own wording, and each names the remedy
    /// that state admits: a pre-pinned id has no conversation to qualify, so
    /// it is told to send a message, and the rest are told to re-assert one.
    #[test]
    fn user_message_distinguishes_every_refusal_state() {
        let cases = [
            (
                ForkDenied::AgentCannotFork {
                    agent: "gemini".into(),
                },
                "Nothing to fork: session 'Legacy Parent' runs agent 'gemini', which has no native fork capability. Forkable agents: claude, codex, opencode.",
            ),
            (
                ForkDenied::NoParentSession,
                "Nothing to fork: session 'Legacy Parent' has no single captured conversation to fork from: it has captured none, or more than one session records this conversation id.",
            ),
            (
                ForkDenied::UnqualifiedParent {
                    pre_pinned: true,
                    recorded: "parent-uuid".into(),
                },
                "Nothing to fork: session 'Legacy Parent' has no captured conversation to fork from. Send it at least one message first.",
            ),
            (
                ForkDenied::UnqualifiedParent {
                    pre_pinned: false,
                    recorded: "parent-uuid".into(),
                },
                "Nothing to fork: session 'Legacy Parent' records conversation 'parent-uuid', which nothing qualifies, so which conversation it names is unknown. Qualify it with `aoe session set-session-id 4f2a8c10 parent-uuid`, and for a pi or omp session add a --store flag naming the absolute path of its transcript.",
            ),
            (
                ForkDenied::UnlaunchedFork,
                "Nothing to fork: session 'Legacy Parent' is a fork that has not launched yet. Start it once, then fork from the child conversation.",
            ),
        ];
        for (denied, expected) in cases {
            assert_eq!(denied.user_message("Legacy Parent", "4f2a8c10"), expected);
        }
    }

    /// A printed remedy has to run as printed, so both values are quoted and
    /// the printed span is one command line. Tokenising it is what a shell
    /// hands to `execve`, and the binary cannot be run from here: this test
    /// binary is not the `aoe` binary.
    #[test]
    fn the_printed_remedy_tokenizes_to_one_runnable_command() {
        let message = ForkDenied::UnqualifiedParent {
            pre_pinned: false,
            recorded: "parent uuid".into(),
        }
        .user_message("Legacy Parent", "4f2a 8c10");
        let command = message
            .split_once('`')
            .and_then(|(_, rest)| rest.split_once('`'))
            .map_or_else(
                || panic!("one quoted remedy in: {message}"),
                |(span, _)| span,
            );
        assert_eq!(
            shell_words::split(command).expect("the remedy tokenizes"),
            [
                "aoe",
                "session",
                "set-session-id",
                "4f2a 8c10",
                "parent uuid"
            ]
        );
    }
}
