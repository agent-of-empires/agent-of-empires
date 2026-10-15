//! The kind of an aoe tmux session, stamped as `@aoe_kind` at creation because
//! name shapes are ambiguous (`aoe_term_Foo_<id8>` is the agent titled
//! `term Foo` and the terminal of `Foo`).

use super::{CONTAINER_TERMINAL_PREFIX, SESSION_PREFIX, TERMINAL_PREFIX, TOOL_PREFIX};

pub(crate) const KIND_OPTION: &str = "@aoe_kind";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionKind {
    Agent,
    Terminal,
    ContainerTerminal,
    Tool,
}

impl SessionKind {
    /// Auxiliary prefixes nest under [`SESSION_PREFIX`], so `Agent` comes last.
    const BY_PREFIX: [(&'static str, Self); 4] = [
        (TERMINAL_PREFIX, Self::Terminal),
        (CONTAINER_TERMINAL_PREFIX, Self::ContainerTerminal),
        (TOOL_PREFIX, Self::Tool),
        (SESSION_PREFIX, Self::Agent),
    ];

    /// Stable: live sessions keep the marker an earlier build stamped.
    pub(crate) const fn as_marker(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Terminal => "term",
            Self::ContainerTerminal => "cterm",
            Self::Tool => "tool",
        }
    }

    pub(crate) fn from_marker(marker: &str) -> Option<Self> {
        Self::BY_PREFIX
            .into_iter()
            .map(|(_, kind)| kind)
            .find(|kind| kind.as_marker() == marker)
    }

    /// Name-shape fallback for sessions without a marker.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::BY_PREFIX
            .into_iter()
            .find(|(prefix, _)| name.starts_with(prefix))
            .map(|(_, kind)| kind)
    }

    /// The marker if it parses, else the name shape.
    pub(crate) fn of(name: &str, marker: Option<&str>) -> Option<Self> {
        marker
            .and_then(Self::from_marker)
            .or_else(|| Self::from_name(name))
    }
}

/// Chain `; set-option -t <target> @aoe_kind <kind>` onto `new-session` so no
/// scan can observe the session unmarked.
pub(crate) fn append_session_kind_args(args: &mut Vec<String>, target: &str, kind: SessionKind) {
    args.extend(
        [
            ";",
            "set-option",
            "-t",
            target,
            KIND_OPTION,
            kind.as_marker(),
        ]
        .map(str::to_string),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID8: &str = "_abcd1234";

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
