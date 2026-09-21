//! State filter shared by the CLI (`aoe list --state`) and the daemon REST API (`GET
//! /api/sessions?state=`) so the two vocabularies cannot drift.

use serde::{Deserialize, Serialize};

use super::Instance;

/// Which "state" of session a caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionScope {
    /// The default in every current caller: sessions that are neither archived nor trashed.
    Live,
    /// Sessions currently in the trash (`remove` but not yet purged).
    Trashed,
    /// Every persisted session, regardless of state.
    All,
}

impl SessionScope {
    /// Does `inst` belong in a listing filtered by `scope`?
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

    #[test]
    fn wire_names_are_lowercase_and_exact() {
        for (wire, want) in [
            ("live", SessionScope::Live),
            ("trashed", SessionScope::Trashed),
            ("all", SessionScope::All),
        ] {
            let json = format!("\"{wire}\"");
            assert_eq!(serde_json::from_str::<SessionScope>(&json).unwrap(), want);
        }
        for bad in ["\"archived\"", "\"LIVE\"", "\"\""] {
            assert!(serde_json::from_str::<SessionScope>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn matches_filters_by_state() {
        let live = Instance::new("live", "/repo");
        let mut archived = Instance::new("arch", "/repo");
        archived.archive();
        let mut trashed = Instance::new("trash", "/repo");
        trashed.trash();

        // (scope, live row, archived row, trashed row)
        let cases = [
            (None, true, true, true),
            (Some(SessionScope::All), true, true, true),
            (Some(SessionScope::Live), true, false, false),
            (Some(SessionScope::Trashed), false, false, true),
        ];
        for (scope, want_live, want_archived, want_trashed) in cases {
            for (inst, want) in [
                (&live, want_live),
                (&archived, want_archived),
                (&trashed, want_trashed),
            ] {
                let got = SessionScope::matches(scope, inst);
                assert_eq!(got, want, "{scope:?} vs {}", inst.title);
            }
        }
    }
}
