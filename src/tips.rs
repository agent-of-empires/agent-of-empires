//! Tips engine: a small registry of "did you know" hints surfaced in the UI.
//!
//! This module is pure data plus selection logic, with no rendering or I/O, so
//! every surface renders from one catalog. The TUI consumes it today; because
//! it lives in the shared lib (not under `tui`), the `serve` server can expose
//! the same catalog to the web dashboard later with no rework.
//!
//! Two kinds of tips:
//! - **Rotation** tips are always eligible and surface passively (the badge +
//!   the tips list). They never interrupt.
//! - **Earned** tips become eligible only once a behavior signal fires, and may
//!   pop once on their own, so a hint shows up exactly when it would help.
//!
//! Seen state lives in `config.app_state.tips_seen` and the on/off preference
//! in `config.session.show_tips`, so both are shared across surfaces and
//! survive restarts.

/// Behavior signals an earned tip's trigger can inspect. Sourced from
/// `config.app_state`; add a field here when a new earned tip needs a new
/// signal.
#[derive(Debug, Clone, Default)]
pub struct TipSignals {
    /// How many times the new-session dialog has been opened while a project
    /// or session was selected. Drives the "new from selection" earned tip
    /// (the discoverability fix for #2262).
    pub new_session_with_selection_count: u32,
    /// Whether the user has already used `N` (new-from-selection). Once true,
    /// the tip teaching it is suppressed; they've discovered the feature.
    pub used_new_from_selection: bool,
}

/// Number of `new_session_with_selection` opens before the "new from
/// selection" tip becomes eligible. Set so a brand-new user isn't nudged on
/// their first session, but someone who keeps opening `n` with a row selected
/// eventually learns about `N`.
pub const NEW_FROM_SELECTION_TIP_THRESHOLD: u32 = 3;

/// When a tip becomes eligible to surface.
pub enum TipTrigger {
    /// Always eligible. Surfaced passively via the badge + tips list; never
    /// pops on its own.
    Rotation,
    /// Eligible only once the predicate (reading [`TipSignals`]) returns true.
    /// Earned tips may pop once, in addition to appearing in the list.
    Earned(fn(&TipSignals) -> bool),
}

/// A single tip.
pub struct Tip {
    /// Stable identity, used as the persistence key in `tips_seen`. Never
    /// reuse or renumber an id, or a user's seen-state would point at the
    /// wrong tip.
    pub id: &'static str,
    /// One-line summary shown in the list (and as the badge's headline).
    pub title: &'static str,
    /// Longer explanation shown when the tip is focused in the list.
    pub body: &'static str,
    /// What makes this tip eligible to surface.
    pub trigger: TipTrigger,
}

impl Tip {
    /// Whether this tip is eligible to surface given the current signals.
    fn is_eligible(&self, signals: &TipSignals) -> bool {
        match self.trigger {
            TipTrigger::Rotation => true,
            TipTrigger::Earned(predicate) => predicate(signals),
        }
    }

    /// Whether this tip is allowed to pop on its own (earned tips only).
    pub fn is_earned(&self) -> bool {
        matches!(self.trigger, TipTrigger::Earned(_))
    }
}

fn earned_new_from_selection(signals: &TipSignals) -> bool {
    // Only nudge users who keep opening `n` with a selection AND haven't yet
    // discovered `N` for themselves.
    !signals.used_new_from_selection
        && signals.new_session_with_selection_count >= NEW_FROM_SELECTION_TIP_THRESHOLD
}

/// The full catalog, in display order.
pub fn catalog() -> &'static [Tip] {
    CATALOG
}

static CATALOG: &[Tip] = &[Tip {
    id: "new-from-selection",
    title: "Reuse the selected session's settings",
    // `{new_from_selection}` is substituted with the live keybinding label by
    // the tips overlay, so it stays correct in strict-hotkey mode (where the
    // chord is Ctrl+N rather than Shift+N).
    body: "Tired of choosing the directory, profile, and group every time? Press \
           {new_from_selection} on the home view to start a new session that inherits \
           all of them from the session you have selected.",
    trigger: TipTrigger::Earned(earned_new_from_selection),
}];

/// Whether `id` is present in the seen list.
fn is_seen(seen: &[String], id: &str) -> bool {
    seen.iter().any(|s| s == id)
}

/// Tips eligible to surface given the current signals, ignoring seen-state, in
/// catalog order. The tips list shows these (seen ones marked); the badge and
/// pops use the `*_unseen` variants below.
pub fn eligible(signals: &TipSignals) -> Vec<&'static Tip> {
    catalog()
        .iter()
        .filter(|tip| tip.is_eligible(signals))
        .collect()
}

/// Tips eligible to surface for a user who has already seen `seen`, given the
/// current signals, in catalog order. Callers should additionally honor the
/// `session.show_tips` setting before showing anything.
pub fn eligible_unseen(seen: &[String], signals: &TipSignals) -> Vec<&'static Tip> {
    eligible(signals)
        .into_iter()
        .filter(|tip| !is_seen(seen, tip.id))
        .collect()
}

/// Count of eligible, unseen tips. Drives the badge.
pub fn unseen_count(seen: &[String], signals: &TipSignals) -> usize {
    eligible_unseen(seen, signals).len()
}

/// The first earned tip that is eligible and unseen, i.e. one that may pop on
/// its own right now. Rotation tips never pop, so they are excluded here.
pub fn next_earned_pop(seen: &[String], signals: &TipSignals) -> Option<&'static Tip> {
    catalog()
        .iter()
        .find(|tip| tip.is_earned() && tip.is_eligible(signals) && !is_seen(seen, tip.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_id(id: &str) -> Option<&'static Tip> {
        catalog().iter().find(|tip| tip.id == id)
    }

    fn signals(count: u32) -> TipSignals {
        TipSignals {
            new_session_with_selection_count: count,
            used_new_from_selection: false,
        }
    }

    #[test]
    fn catalog_ids_are_unique_and_nonempty() {
        let ids: Vec<&str> = catalog().iter().map(|t| t.id).collect();
        assert!(!ids.is_empty());
        for tip in catalog() {
            assert!(!tip.id.is_empty(), "every tip needs an id");
            assert!(!tip.title.is_empty(), "every tip needs a title");
            assert!(!tip.body.is_empty(), "every tip needs a body");
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "tip ids must be unique");
    }

    #[test]
    fn earned_tip_suppressed_once_n_used() {
        let tip = by_id("new-from-selection").unwrap();
        // Over threshold but the user already discovered N: stay ineligible.
        let used = TipSignals {
            new_session_with_selection_count: NEW_FROM_SELECTION_TIP_THRESHOLD + 5,
            used_new_from_selection: true,
        };
        assert!(!tip.is_eligible(&used));
        assert_eq!(unseen_count(&[], &used), 0);
        assert!(next_earned_pop(&[], &used).is_none());
    }

    #[test]
    fn earned_tip_gates_on_threshold() {
        let tip = by_id("new-from-selection").unwrap();
        assert!(tip.is_earned());
        assert!(!tip.is_eligible(&signals(0)));
        assert!(!tip.is_eligible(&signals(NEW_FROM_SELECTION_TIP_THRESHOLD - 1)));
        assert!(tip.is_eligible(&signals(NEW_FROM_SELECTION_TIP_THRESHOLD)));
        assert!(tip.is_eligible(&signals(NEW_FROM_SELECTION_TIP_THRESHOLD + 5)));
    }

    #[test]
    fn unseen_count_tracks_eligibility_and_seen() {
        // The only tip is earned, so below threshold nothing is eligible.
        assert_eq!(unseen_count(&[], &signals(0)), 0);

        // At threshold it becomes eligible and unseen.
        assert_eq!(
            unseen_count(&[], &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)),
            1
        );

        // Once seen, it drops back out of the count.
        let seen = vec!["new-from-selection".to_string()];
        assert_eq!(
            unseen_count(&seen, &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)),
            0
        );
    }

    #[test]
    fn next_earned_pop_only_when_eligible_and_unseen() {
        // Below threshold: nothing to pop.
        assert!(next_earned_pop(&[], &signals(0)).is_none());

        // At threshold: the new-from-selection tip pops.
        let pop = next_earned_pop(&[], &signals(NEW_FROM_SELECTION_TIP_THRESHOLD));
        assert_eq!(pop.map(|t| t.id), Some("new-from-selection"));

        // Once seen, it no longer pops even when eligible.
        let seen = vec!["new-from-selection".to_string()];
        assert!(next_earned_pop(&seen, &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)).is_none());
    }
}
