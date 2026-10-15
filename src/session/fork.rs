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
        /// The parent row's own native agent when the parent's binding is
        /// unattributed, so the surface building the child can hold the launch to
        /// the parent's identity: the launch-time check skips an unattributed
        /// binding, so nothing else would. `None` for a qualified binding, whose
        /// identity the launch already checks.
        unattributed_parent_agent: Option<String>,
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
    /// `preallocated` is true when the binding's provenance is `preallocated`:
    /// the launch reserved the id and no qualified binding was ever published
    /// for it, which is not the same as no conversation having run (the native
    /// command is launched before the binding is written), so the remedy is a
    /// message rather than an assertion about a conversation that does not
    /// exist. `recorded` is the id the parent carries, and it is the value a
    /// qualification re-asserts.
    UnqualifiedParent {
        preallocated: bool,
        recorded: String,
    },
    /// The parent is a fork whose launch has not happened, so it holds no
    /// conversation of its own: the first launch is the fork.
    UnlaunchedFork,
}

impl ForkDenied {
    /// The refusal phrased for a user, so the wording and its remedy live in
    /// one place. `title` names the session as the user knows it; `id` is what
    /// the remedy carries, because `resolve_session` resolves a title to
    /// whichever row it meets first, so an id is the only session name a
    /// printed command can act on. `profile` is the store the parent lives in,
    /// which the remedy has to name: `set-session-id` opens only the store
    /// the profile names.
    pub fn user_message(&self, title: &str, id: &str, profile: &str) -> String {
        match self {
            Self::AgentCannotFork { agent } => format!(
                "Nothing to fork: session '{title}' runs agent '{agent}', which has no native fork capability. Forkable agents: claude, codex, opencode, pi."
            ),
            Self::NoParentSession => format!(
                "Nothing to fork: session '{title}' has no single captured conversation to fork from: it has captured none, or more than one session records this conversation id."
            ),
            Self::UnqualifiedParent { preallocated: false, recorded } => format!(
                "Nothing to fork: session '{title}' records conversation '{recorded}', which nothing qualifies, so which conversation it names is unknown. Qualify it with `{command}`.",
                command = qualify_command(id, recorded, profile)
            ),
            Self::UnqualifiedParent { preallocated: true, .. } => format!(
                "Nothing to fork: session '{title}' has no qualified conversation to fork from. Send it at least one message first."
            ),
            Self::UnlaunchedFork => format!(
                "Nothing to fork: session '{title}' is a fork that has not launched yet. Start it once, then fork from the child conversation."
            ),
        }
    }
}

/// The one command that re-asserts a recorded id. It names the profile,
/// because `set-session-id` opens only the store that profile names, and a
/// remedy run against the default profile would qualify nothing. It carries
/// the session id, not the title, because `resolve_session` resolves a title
/// to whichever row it meets first, and each value is quoted so one holding a
/// space stays one argument. Backticks delimit it in prose: the apostrophe
/// would be one more shell metacharacter to demangle, the backtick only marks
/// a span.
fn qualify_command(id: &str, recorded: &str, profile: &str) -> String {
    format!(
        "aoe -p {} session set-session-id {} {}",
        shell_words::quote(profile),
        shell_words::quote(id),
        shell_words::quote(recorded)
    )
}

/// The conversation an explicit fork would carry, with the evidence for it
/// spelled out: a binding proves which native conversation the id names, a
/// bare recorded id proves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkParentRef<'a> {
    /// The id has a binding whose provenance says how far it is trusted, so an
    /// unqualified one is still evidence of something.
    Bound(&'a crate::session::ConversationBinding),
    /// The row records an id a migration left unattributed: no binding
    /// qualifies it, so the store comes from the context this launch resolves.
    /// `agent` is the row's own native agent, which decides whether a fork
    /// can be dispatched at all.
    Unattributed {
        binding: &'a crate::session::ConversationBinding,
        agent: &'a str,
    },
    /// The id is recorded with no binding behind it, so no record names the
    /// agent, store or directory it belongs to.
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
            Self::Bound(binding) | Self::Unattributed { binding, .. } => Some(&binding.session_id),
            Self::Recorded(session_id) => Some(session_id),
            Self::Unlaunched => None,
        }
    }

    /// The binding, when the parent still has one.
    pub fn binding(self) -> Option<&'a crate::session::ConversationBinding> {
        match self {
            Self::Bound(binding) | Self::Unattributed { binding, .. } => Some(binding),
            Self::Recorded(_) | Self::Unlaunched => None,
        }
    }

    /// Whether the binding is strong enough to fork on.
    pub fn is_known(self) -> bool {
        self.binding()
            .is_some_and(crate::session::ConversationBinding::is_known)
    }

    /// How admissible this parent is as a fork source: a qualified binding
    /// first, a row a migration left unattributed next, and a candidate
    /// nothing qualifies last. Several rows can record one id, so ranking by
    /// this before breaking ties on `id` keeps a refusal from being decided
    /// by which row the store happened to return first.
    pub fn admissibility(self) -> u8 {
        match self {
            Self::Bound(binding) if binding.is_known() => 0,
            Self::Unattributed { .. } => 1,
            _ => 2,
        }
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
///
/// Whether an agent can fork does not depend on which generation is installed,
/// so these two predicates read the descriptor directly instead of probing the
/// binary. They answer per row while a menu or a dashboard renders, and a probe
/// there would spawn a subprocess on the render thread.
pub fn structured_fork_capable(tool: &str, agent_name: Option<&str>) -> bool {
    let resolved = agent_name.filter(|s| !s.is_empty()).unwrap_or(tool);
    builtin_acp_registry().get(resolved).is_some()
        && get_agent(resolved).is_some_and(|agent| match agent.fork_strategy {
            ForkStrategy::EitherGeneration { legacy, current } => {
                matches!(legacy, ForkStrategy::ClaudeFork)
                    && matches!(current, ForkStrategy::ClaudeFork)
            }
            strategy => matches!(strategy, ForkStrategy::ClaudeFork),
        })
}

/// Whether a canonical native agent supports terminal forking. An agent that
/// declares two generations is forkable when either arm is, since the installed
/// binary sits on one side or the other and refusing on the strength of the arm
/// that does not apply would hide a capability the user has.
pub fn terminal_agent_can_fork(agent: &str) -> bool {
    get_agent(agent).is_some_and(|def| match def.fork_strategy {
        ForkStrategy::EitherGeneration { legacy, current } => {
            !matches!(legacy, ForkStrategy::Unsupported)
                || !matches!(current, ForkStrategy::Unsupported)
        }
        strategy => !matches!(strategy, ForkStrategy::Unsupported),
    })
}

/// Hold a fork child to the parent's identity when the parent is an
/// unattributed id, which the launch cannot identity-check. `launched` is the
/// agent the child's own command and profile actually resolve to, not the
/// tool label it was asked for: a declared wrapper or a direct native command
/// names another agent, and only the resolution proves which. `Ok` for a
/// qualified parent, whose identity the launch already checks.
pub fn ensure_child_matches_parent_agent(
    parent_agent: Option<&str>,
    launched: &str,
) -> Result<(), String> {
    let Some(parent_agent) = parent_agent else {
        return Ok(());
    };
    if parent_agent == launched {
        return Ok(());
    }
    Err(format!(
        "Nothing to fork: the new session would launch agent '{launched}', but the parent \
         session runs agent '{parent_agent}', and a fork can only carry a conversation from the \
         agent that recorded it."
    ))
}

/// Decide whether a terminal session can be forked, and produce its one-shot seed.
pub fn terminal_fork_seed(
    parent: Option<ForkParentRef<'_>>,
    child_session_id: String,
) -> Result<ForkSeed, ForkDenied> {
    // One refusal for every id nothing qualifies: only a binding can prove the
    // id was reserved and never published, and a row with no binding proves
    // nothing at all.
    let unqualified = |binding: Option<&crate::session::ConversationBinding>, recorded: &str| {
        ForkDenied::UnqualifiedParent {
            preallocated: binding.is_some_and(|binding| {
                matches!(binding.provenance, ConversationProvenance::Preallocated)
            }),
            recorded: recorded.to_string(),
        }
    };
    // A qualified binding is the whole gate, and so is a binding a migration
    // left unattributed, which the resume path also accepts.
    let (parent, agent, unattributed_parent_agent) = match parent {
        Some(ForkParentRef::Bound(parent)) if parent.is_known() => (
            parent,
            parent
                .execution
                .as_ref()
                .map(|execution| execution.agent.as_str()),
            None,
        ),
        Some(ForkParentRef::Unattributed { binding, agent }) => {
            (binding, Some(agent), Some(agent.to_owned()))
        }
        Some(ForkParentRef::Bound(parent)) => {
            return Err(unqualified(Some(parent), &parent.session_id));
        }
        Some(ForkParentRef::Recorded(recorded)) => return Err(unqualified(None, recorded)),
        Some(ForkParentRef::Unlaunched) => return Err(ForkDenied::UnlaunchedFork),
        None => return Err(ForkDenied::NoParentSession),
    };
    // A known binding always records an execution, so both admitted arms name
    // an agent; one that does not proves nothing and stays refused.
    let Some(agent) = agent else {
        return Err(unqualified(Some(parent), &parent.session_id));
    };
    if !terminal_agent_can_fork(agent) {
        return Err(ForkDenied::AgentCannotFork {
            agent: agent.to_owned(),
        });
    }
    Ok(ForkSeed::Terminal {
        parent: Box::new(parent.clone()),
        child_session_id,
        unattributed_parent_agent,
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
    /// and only a preallocated one is told to send a message, while a fork whose
    /// launch has not happened is refused as itself.
    #[test]
    fn fork_reports_a_recorded_but_unqualified_parent_separately() {
        for (provenance, preallocated) in [
            (ConversationProvenance::Preallocated, true),
            (ConversationProvenance::Unknown, false),
        ] {
            let parent = bound(provenance.clone());
            assert_eq!(
                terminal_fork_seed(Some(ForkParentRef::Bound(&parent)), "child-uuid".into()),
                Err(ForkDenied::UnqualifiedParent {
                    preallocated,
                    recorded: "parent-uuid".into(),
                }),
                "{provenance:?}"
            );
        }
        // A binding nothing qualified and no binding at all are one refusal:
        // neither proves a qualified conversation was ever published for the id.
        assert_eq!(
            terminal_fork_seed(
                Some(ForkParentRef::Recorded("legacy-uuid")),
                "child-uuid".into()
            ),
            Err(ForkDenied::UnqualifiedParent {
                preallocated: false,
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

    #[test]
    fn pi_forks_terminal_conversations_but_not_structured_sessions() {
        let mut parent = bound(ConversationProvenance::Observed);
        parent.execution.as_mut().unwrap().agent = "pi".into();
        let seed = terminal_fork_seed(Some(ForkParentRef::Bound(&parent)), "child-uuid".into())
            .expect("a qualified Pi conversation can fork");
        assert_eq!(
            seed,
            ForkSeed::Terminal {
                parent: Box::new(parent.clone()),
                child_session_id: "child-uuid".into(),
                unattributed_parent_agent: None,
            }
        );
        assert!(!structured_fork_capable("pi", None));
        assert!(!structured_fork_capable("custom", Some("pi")));
        assert!(structured_fork_capable("claude", None));
    }

    /// A binding a migration left unattributed forks when the row's own agent
    /// can, and is refused naming that agent when it cannot; the binding is
    /// carried through untouched, so the child still launches unattributed.
    #[test]
    fn an_unattributed_parent_forks_on_the_rows_own_agent() {
        let binding = ConversationBinding::unknown("parent-uuid");
        for (agent, forked) in [("claude", true), ("pi", true), ("gemini", false)] {
            let seed = terminal_fork_seed(
                Some(ForkParentRef::Unattributed {
                    binding: &binding,
                    agent,
                }),
                "child-uuid".into(),
            );
            match (seed, forked) {
                (Ok(ForkSeed::Terminal { parent, .. }), true) => {
                    assert_eq!(parent.as_ref(), &binding, "the binding is carried as is");
                }
                (Err(ForkDenied::AgentCannotFork { agent: named }), false) => {
                    assert_eq!(named, agent, "the refusal names the row's own agent");
                }
                (seed, forked) => panic!("{agent} forked={forked}: {seed:?}"),
            }
        }
    }

    /// Every refusal state names the remedy that comes back to it, and no
    /// other state's remedy: a preallocated id has no conversation to qualify,
    /// so it is told to send a message, and the rest are told to re-assert one.
    #[test]
    fn user_message_names_the_remedy_of_its_own_state_only() {
        let (agent_cannot_fork, no_parent, send_a_message, re_assert, unlaunched) = (
            "no native fork capability",
            "captured none",
            "Send it at least one message first",
            "set-session-id",
            "has not launched yet",
        );
        let cases = [
            (
                ForkDenied::AgentCannotFork {
                    agent: "gemini".into(),
                },
                vec![agent_cannot_fork, "Forkable agents: claude"],
            ),
            (ForkDenied::NoParentSession, vec![no_parent]),
            (
                ForkDenied::UnqualifiedParent {
                    preallocated: true,
                    recorded: "parent-uuid".into(),
                },
                vec![send_a_message],
            ),
            (
                ForkDenied::UnqualifiedParent {
                    preallocated: false,
                    recorded: "parent-uuid".into(),
                },
                vec![re_assert, "parent-uuid"],
            ),
            (ForkDenied::UnlaunchedFork, vec![unlaunched]),
        ];
        let remedies = [
            agent_cannot_fork,
            no_parent,
            send_a_message,
            re_assert,
            unlaunched,
        ];
        for (denied, own) in &cases {
            let message = denied.user_message("Legacy Parent", "4f2a8c10", "default");
            for remedy in own {
                assert!(
                    message.contains(remedy),
                    "{denied:?} names {remedy:?}: {message}"
                );
            }
            for remedy in remedies {
                if !own.contains(&remedy) {
                    assert!(
                        !message.contains(remedy),
                        "{denied:?} names another state's remedy {remedy:?}: {message}"
                    );
                }
            }
        }
    }

    /// A printed remedy has to reach the shell as one command line, so both
    /// values are quoted. Tokenising it is what a shell hands to `execve`;
    /// that the CLI still accepts this command line is the e2e's property.
    #[test]
    fn the_printed_remedy_tokenizes_to_one_command_line() {
        let message = ForkDenied::UnqualifiedParent {
            preallocated: false,
            recorded: "parent uuid".into(),
        }
        .user_message("Legacy Parent", "4f2a 8c10", "my profile");
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
                "-p",
                "my profile",
                "session",
                "set-session-id",
                "4f2a 8c10",
                "parent uuid"
            ]
        );
    }
}
