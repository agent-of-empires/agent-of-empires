//! What kind of aoe session a live tmux session is, recorded on the session
//! itself rather than inferred from its name.
//!
//! Every kind's name is `<prefix><sanitized title><suffix>`, so a title that
//! sanitizes under another kind's prefix produces a name of that kind's shape:
//! `aoe_term_Foo_<id8>` is both the agent name for title `term Foo` and the
//! paired-terminal name for title `Foo`. Name shape alone cannot separate
//! them, which cost the ambiguous rows their session-id poller (#3888) and,
//! the other way around, let a poller follow a paired terminal that outlived
//! its agent (#3880).
//!
//! `@aoe_kind` is written once at creation and moves with the session through
//! renames, so a scan that reads it never has to guess. It joins the existing
//! `@aoe_*` user options (`@aoe_title`, `@aoe_branch`, `@aoe_sandbox`), which
//! are aoe's own namespace and never override a user's tmux config.

use super::{CONTAINER_TERMINAL_PREFIX, SESSION_PREFIX, TERMINAL_PREFIX, TOOL_PREFIX};

/// The tmux user option carrying [`SessionKind`].
pub(crate) const KIND_OPTION: &str = "@aoe_kind";

/// The kind of aoe tmux session, as recorded by [`KIND_OPTION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionKind {
    /// The pane the agent runs in, and the only kind a session-id poller can
    /// follow.
    Agent,
    Terminal,
    ContainerTerminal,
    Tool,
}

impl SessionKind {
    /// The stored marker value. Stable: a session created by an earlier build
    /// keeps whatever it was stamped with for its whole life.
    pub(crate) const fn as_marker(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Terminal => "term",
            Self::ContainerTerminal => "cterm",
            Self::Tool => "tool",
        }
    }

    /// Parse a stored marker. `None` for an unset option (empty) or a
    /// value this build does not know.
    pub(crate) fn from_marker(marker: &str) -> Option<Self> {
        match marker {
            "agent" => Some(Self::Agent),
            "term" => Some(Self::Terminal),
            "cterm" => Some(Self::ContainerTerminal),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }

    /// Classify by name shape, for a session with no marker: one created
    /// before this option existed, or whose `set-option` did not land.
    ///
    /// This is the ambiguous answer the marker replaces, kept as the fallback
    /// so an upgrade does not have to restart every running session. The
    /// auxiliary prefixes nest under [`SESSION_PREFIX`], so they are tested
    /// first and the agent arm is what is left.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        if name.starts_with(TERMINAL_PREFIX) {
            Some(Self::Terminal)
        } else if name.starts_with(CONTAINER_TERMINAL_PREFIX) {
            Some(Self::ContainerTerminal)
        } else if name.starts_with(TOOL_PREFIX) {
            Some(Self::Tool)
        } else if name.starts_with(SESSION_PREFIX) {
            Some(Self::Agent)
        } else {
            None
        }
    }

    /// The kind of a live session: the marker it was created with, else its
    /// name shape. An unparsable marker is treated as absent rather than as
    /// evidence of anything.
    pub(crate) fn of(name: &str, marker: Option<&str>) -> Option<Self> {
        marker
            .and_then(Self::from_marker)
            .or_else(|| Self::from_name(name))
    }
}

/// Append `; set-option -t <target> @aoe_kind <kind>` to an in-flight
/// `new-session` argument list, so a session is marked in the same tmux
/// invocation that creates it.
///
/// Chained rather than sent afterwards for the same reason
/// `append_remain_on_exit_args` is: a second call is a second fork, and the
/// gap between them is a window where a scan sees the session unmarked and
/// falls back to guessing from its name.
pub(crate) fn append_session_kind_args(args: &mut Vec<String>, target: &str, kind: SessionKind) {
    args.extend([
        ";".to_string(),
        "set-option".to_string(),
        "-t".to_string(),
        target.to_string(),
        KIND_OPTION.to_string(),
        kind.as_marker().to_string(),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID8: &str = "_abcd1234";

    /// The marker is what the name shape cannot say. A title sanitizing to
    /// `term_...` gives an agent session a paired terminal's shape, and a
    /// paired terminal of a plainly titled row gives the same string, so both
    /// directions have to come from the marker.
    #[test]
    fn marker_outranks_the_name_shape_in_both_directions() {
        let ambiguous = format!("{TERMINAL_PREFIX}Foo{ID8}");

        assert_eq!(
            SessionKind::of(&ambiguous, Some("agent")),
            Some(SessionKind::Agent),
            "an agent titled `term Foo` is an agent"
        );
        assert_eq!(
            SessionKind::of(&ambiguous, Some("term")),
            Some(SessionKind::Terminal),
            "the paired terminal of a row titled `Foo` is a terminal"
        );
        assert_eq!(
            SessionKind::of(&ambiguous, None),
            Some(SessionKind::Terminal),
            "unmarked keeps the pre-marker guess"
        );
    }

    #[test]
    fn unmarked_and_unparsable_fall_back_to_the_name_shape() {
        let cases = [
            (format!("{SESSION_PREFIX}Vikings{ID8}"), SessionKind::Agent),
            (
                format!("{TERMINAL_PREFIX}Vikings{ID8}"),
                SessionKind::Terminal,
            ),
            (
                format!("{CONTAINER_TERMINAL_PREFIX}Vikings{ID8}"),
                SessionKind::ContainerTerminal,
            ),
            (
                format!("{TOOL_PREFIX}claude_Vikings{ID8}"),
                SessionKind::Tool,
            ),
        ];
        for (name, expected) in cases {
            assert_eq!(SessionKind::of(&name, None), Some(expected), "{name}");
            assert_eq!(
                SessionKind::of(&name, Some("")),
                Some(expected),
                "an unset option reads as empty, not as a kind: {name}"
            );
            assert_eq!(
                SessionKind::of(&name, Some("nonsense")),
                Some(expected),
                "an unparsable marker is not evidence: {name}"
            );
        }
        assert_eq!(SessionKind::of("someone-elses-session", None), None);
    }

    #[test]
    fn markers_round_trip() {
        for kind in [
            SessionKind::Agent,
            SessionKind::Terminal,
            SessionKind::ContainerTerminal,
            SessionKind::Tool,
        ] {
            assert_eq!(
                SessionKind::of("someone-elses-session", Some(kind.as_marker())),
                Some(kind),
            );
        }
    }
}
