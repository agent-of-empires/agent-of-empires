//! State filter shared by the CLI (`aoe list --state`) and the daemon
//! REST API (`GET /api/sessions?state=`) so the two vocabularies cannot
//! drift. See #3350 for the CLI parity motivation and #3156/#3187 for
//! the API's original design.

use serde::Deserialize;

use super::Instance;

/// Which "state" of session a caller wants. The variants are the wire
/// vocabulary (`state=live|trashed|all`); `#[serde(rename_all = "lowercase")]`
/// pins that and rejects any other value at deserialize time so a typo
/// surfaces as an error rather than silently returning every session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    /// Does `inst` belong in a listing filtered by `scope`? `None` (no filter
    /// specified by the caller) behaves like `All`, matching the historical
    /// unfiltered behavior so nothing breaks.
    pub fn matches(scope: Option<SessionScope>, inst: &Instance) -> bool {
        match scope {
            None | Some(SessionScope::All) => true,
            Some(SessionScope::Live) => !inst.is_archived() && !inst.is_trashed(),
            Some(SessionScope::Trashed) => inst.is_trashed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lowercase wire names are how the REST API and the CLI both parse
    /// the value. Pin them so a rename can't drift out of sync.
    #[test]
    fn deserializes_lowercase_wire_names() {
        assert_eq!(
            serde_json::from_str::<SessionScope>("\"live\"").unwrap(),
            SessionScope::Live
        );
        assert_eq!(
            serde_json::from_str::<SessionScope>("\"trashed\"").unwrap(),
            SessionScope::Trashed
        );
        assert_eq!(
            serde_json::from_str::<SessionScope>("\"all\"").unwrap(),
            SessionScope::All
        );
    }

    #[test]
    fn rejects_unrecognized_value() {
        assert!(serde_json::from_str::<SessionScope>("\"archived\"").is_err());
        assert!(serde_json::from_str::<SessionScope>("\"LIVE\"").is_err());
        assert!(serde_json::from_str::<SessionScope>("\"\"").is_err());
    }

    #[test]
    fn matches_no_scope_returns_everything() {
        let mut live = Instance::new("live", "/repo");
        assert!(SessionScope::matches(None, &live));
        live.archive();
        assert!(SessionScope::matches(None, &live));
    }

    #[test]
    fn matches_live_excludes_archived_and_trashed() {
        let live = Instance::new("live", "/repo");
        let mut archived = Instance::new("arch", "/repo");
        archived.archive();
        let mut trashed = Instance::new("trash", "/repo");
        trashed.trash();

        assert!(SessionScope::matches(Some(SessionScope::Live), &live));
        assert!(!SessionScope::matches(Some(SessionScope::Live), &archived));
        assert!(!SessionScope::matches(Some(SessionScope::Live), &trashed));
    }

    #[test]
    fn matches_trashed_is_trashed_only() {
        let mut trashed = Instance::new("t", "/repo");
        trashed.trash();
        let mut archived = Instance::new("a", "/repo");
        archived.archive();

        assert!(SessionScope::matches(Some(SessionScope::Trashed), &trashed));
        assert!(!SessionScope::matches(
            Some(SessionScope::Trashed),
            &archived
        ));
    }
}
