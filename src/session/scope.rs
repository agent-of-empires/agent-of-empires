//! State filter shared by the CLI (`aoe list --state`) and the daemon
//! REST API (`GET /api/sessions?state=`) so the two vocabularies cannot
//! drift. See #3350 for the CLI parity motivation and #3156/#3187 for
//! the API's original design.

use serde::{Deserialize, Serialize};

/// Which "state" of session a caller wants. The variants are the wire
/// vocabulary (`state=live|trashed|all`); `#[serde(rename_all = "lowercase")]`
/// pins that and rejects any other value at deserialize time so a typo
/// surfaces as an error rather than silently returning every session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionScope {
    /// The default in every current caller: sessions that are neither
    /// archived nor trashed. Matches what a user thinks of as "my active
    /// sessions".
    Live,
    /// Sessions currently in the trash (`remove` but not yet purged). Kept
    /// separate because #3156 flagged that an external supervisor keying on
    /// "does my session still exist" would otherwise treat a trashed row as
    /// alive.
    Trashed,
    /// Every persisted session, regardless of state. Used by the dashboard's
    /// client-side Trash view (which still filters locally, #3187) and by
    /// tests that need to count everything.
    All,
}

impl SessionScope {
    /// An omitted scope includes every persisted session.
    pub fn matches(scope: Option<SessionScope>, archived: bool, trashed: bool) -> bool {
        match scope {
            None | Some(SessionScope::All) => true,
            Some(SessionScope::Live) => !archived && !trashed,
            Some(SessionScope::Trashed) => trashed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_selects_live_and_trashed_states() {
        let states = [(false, false), (true, false), (false, true), (true, true)];
        for (scope, expected) in [
            (None, [true, true, true, true]),
            (Some(SessionScope::All), [true, true, true, true]),
            (Some(SessionScope::Live), [true, false, false, false]),
            (Some(SessionScope::Trashed), [false, false, true, true]),
        ] {
            for ((archived, trashed), expected) in states.into_iter().zip(expected) {
                assert_eq!(SessionScope::matches(scope, archived, trashed), expected);
            }
        }
    }
}
