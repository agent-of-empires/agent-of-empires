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

/// Which surface a tip is meant for. A tip lists every surface it applies to in
/// [`Tip::surfaces`], so keyboard-only hints (the `N` shortcut) stay out of the
/// web dashboard and web-only hints (installing the PWA) stay out of the TUI.
/// All eligibility queries take a surface so each surface only ever sees its
/// own tips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipSurface {
    /// The terminal UI.
    Tui,
    /// The web dashboard (`aoe serve`).
    Web,
}

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
    /// Surfaces this tip applies to. A tip never shows on a surface it doesn't
    /// list, so a query for one surface can't leak another surface's tips.
    pub surfaces: &'static [TipSurface],
}

impl Tip {
    /// Whether this tip is eligible to surface on `surface` given the current
    /// signals: it must apply to the surface and its trigger must fire.
    fn is_eligible(&self, surface: TipSurface, signals: &TipSignals) -> bool {
        self.surfaces.contains(&surface)
            && match self.trigger {
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

static CATALOG: &[Tip] = &[
    Tip {
        id: "new-from-selection",
        title: "Reuse the selected session's settings",
        // `{new_from_selection}` is substituted with the live keybinding label
        // by the tips overlay, so it stays correct in strict-hotkey mode (where
        // the chord is Ctrl+N rather than Shift+N).
        body: "Tired of choosing the directory, profile, and group every time? Press \
               {new_from_selection} on the home view to start a new session that inherits \
               all of them from the session you have selected.",
        trigger: TipTrigger::Earned(earned_new_from_selection),
        // Teaches a keyboard shortcut, so it only makes sense in the TUI.
        surfaces: &[TipSurface::Tui],
    },
    Tip {
        id: "install-dashboard-pwa",
        title: "Install the dashboard as an app",
        // Browser-neutral wording: the engine can't know the user's browser or
        // whether the dashboard is already installed.
        body: "You can install the dashboard as an app for quick access. In your browser, \
               use the install option (Install Agent of Empires in Chrome, or Add to Home \
               Screen on iOS) to keep it one tap away and keep notifications working.",
        trigger: TipTrigger::Rotation,
        // About the web dashboard's PWA install, irrelevant to the TUI.
        surfaces: &[TipSurface::Web],
    },
    // Web feature-discovery tips. These describe web gestures (right-click,
    // pickers, the wizard), so they are web-only; the TUI teaches the same
    // features through its help screen and the keyboard tips below.
    Tip {
        id: "pin-sessions",
        title: "Keep important sessions on top",
        body: "Right-click a session in the sidebar (long-press on touch) and choose Pin to \
               float it to the top of every sort. Unpin it the same way.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    Tip {
        id: "archive-sessions",
        title: "Tuck finished sessions away",
        body: "Right-click a session and choose Archive to stop it and tuck it into the \
               \"Snoozed & archived\" footer at the bottom of the sidebar. Sending it a \
               message brings it right back.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    Tip {
        id: "snooze-sessions",
        title: "Snooze a session for later",
        body: "Right-click a session, choose Snooze, and pick a duration from 1 hour up to \
               1 week. It stays hidden until the timer runs out or you send it a message.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    Tip {
        id: "group-sessions",
        title: "Organize sessions into groups",
        body: "Use the grouping toggle in the sidebar to switch between By repo, By group, \
               and By repo and group. Right-click a session and choose Edit group to file it \
               under any name you like.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    Tip {
        id: "sort-sidebar",
        title: "Sort the sidebar your way",
        body: "The sort picker offers Manual, where you drag the rows into any order \
               yourself, Recent activity, and Attention, which floats the sessions that need \
               you to the top.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    Tip {
        id: "scratch-sessions",
        title: "Spin up a scratch session",
        body: "Need a throwaway? Toggle \"Skip project folder\" in the new-session wizard \
               (or press Ctrl+Shift+N) to start a session with no repo. AoE makes a temp \
               directory and cleans it up when you delete the session.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    Tip {
        id: "multi-repo-sessions",
        title: "Drive several repos at once",
        body: "Save your repos as projects, then multi-select them in the new-session wizard \
               to give one agent a worktree in every repo on a shared branch.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
    // TUI keyboard-shortcut tips. Keys are written as `{placeholder}` and the
    // tips overlay substitutes the live chord (correct in strict-hotkey mode);
    // see `resolve_body` in `src/tui/dialogs/tips.rs`.
    Tip {
        id: "tui-core-views",
        title: "Switch views fast",
        body: "Toggle the agent and terminal panes with {toggle_view}, open the diff with \
               {diff}, jump to settings with {settings}, and open this help any time with \
               {help}.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Tui],
    },
    Tip {
        id: "tui-triage",
        title: "Triage from the keyboard",
        body: "Cycle the sort with {sort} and grouping with {group}, and archive a session \
               with {archive}. Favorite with {favorite} to keep a session within reach; in \
               Attention sort, snooze with {snooze}.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Tui],
    },
    Tip {
        id: "tui-power",
        title: "Power moves",
        body: "Ctrl+K opens the command palette, {serve} exposes the dashboard for remote \
               access, and {tool_session} opens a tool session like lazygit or yazi.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Tui],
    },
];

/// Whether `id` is the id of a tip in the catalog. Used to reject unknown ids
/// before persisting them to the shared seen list.
pub fn id_in_catalog(id: &str) -> bool {
    catalog().iter().any(|tip| tip.id == id)
}

/// Whether `id` is present in the seen list.
fn is_seen(seen: &[String], id: &str) -> bool {
    seen.iter().any(|s| s == id)
}

/// Tips eligible to surface on `surface` given the current signals, ignoring
/// seen-state, in catalog order. The tips list shows these (seen ones marked);
/// the badge and pops use the `*_unseen` variants below.
pub fn eligible(surface: TipSurface, signals: &TipSignals) -> Vec<&'static Tip> {
    catalog()
        .iter()
        .filter(|tip| tip.is_eligible(surface, signals))
        .collect()
}

/// Tips eligible to surface on `surface` for a user who has already seen
/// `seen`, given the current signals, in catalog order. Callers should
/// additionally honor the `session.show_tips` setting before showing anything.
pub fn eligible_unseen(
    surface: TipSurface,
    seen: &[String],
    signals: &TipSignals,
) -> Vec<&'static Tip> {
    eligible(surface, signals)
        .into_iter()
        .filter(|tip| !is_seen(seen, tip.id))
        .collect()
}

/// Count of eligible, unseen tips on `surface`. Drives the badge.
pub fn unseen_count(surface: TipSurface, seen: &[String], signals: &TipSignals) -> usize {
    eligible_unseen(surface, seen, signals).len()
}

/// The first earned tip eligible and unseen on `surface`, i.e. one that may pop
/// on its own right now. Rotation tips never pop, so they are excluded here.
pub fn next_earned_pop(
    surface: TipSurface,
    seen: &[String],
    signals: &TipSignals,
) -> Option<&'static Tip> {
    catalog()
        .iter()
        .find(|tip| tip.is_earned() && tip.is_eligible(surface, signals) && !is_seen(seen, tip.id))
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

    fn web_unseen_ids(seen: &[String], signals: &TipSignals) -> Vec<&'static str> {
        eligible_unseen(TipSurface::Web, seen, signals)
            .iter()
            .map(|t| t.id)
            .collect()
    }

    #[test]
    fn earned_tip_suppressed_once_n_used() {
        let tip = by_id("new-from-selection").unwrap();
        // Over threshold but the user already discovered N: stay ineligible.
        let used = TipSignals {
            new_session_with_selection_count: NEW_FROM_SELECTION_TIP_THRESHOLD + 5,
            used_new_from_selection: true,
        };
        assert!(!tip.is_eligible(TipSurface::Tui, &used));
        // The earned tip drops out, but rotation TUI tips remain.
        let unseen: Vec<&str> = eligible_unseen(TipSurface::Tui, &[], &used)
            .iter()
            .map(|t| t.id)
            .collect();
        assert!(!unseen.contains(&"new-from-selection"));
        assert!(next_earned_pop(TipSurface::Tui, &[], &used).is_none());
    }

    #[test]
    fn earned_tip_gates_on_threshold() {
        let tip = by_id("new-from-selection").unwrap();
        assert!(tip.is_earned());
        assert!(!tip.is_eligible(TipSurface::Tui, &signals(0)));
        assert!(!tip.is_eligible(
            TipSurface::Tui,
            &signals(NEW_FROM_SELECTION_TIP_THRESHOLD - 1)
        ));
        assert!(tip.is_eligible(TipSurface::Tui, &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)));
        assert!(tip.is_eligible(
            TipSurface::Tui,
            &signals(NEW_FROM_SELECTION_TIP_THRESHOLD + 5)
        ));
    }

    #[test]
    fn unseen_count_tracks_eligibility_and_seen() {
        // Earning the N tip adds exactly one to the TUI count, regardless of how
        // many rotation tips ship alongside it.
        let base = unseen_count(TipSurface::Tui, &[], &signals(0));
        assert_eq!(
            unseen_count(
                TipSurface::Tui,
                &[],
                &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)
            ),
            base + 1
        );

        // Once seen, it drops back to the baseline.
        let seen = vec!["new-from-selection".to_string()];
        assert_eq!(
            unseen_count(
                TipSurface::Tui,
                &seen,
                &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)
            ),
            base
        );
    }

    #[test]
    fn next_earned_pop_only_when_eligible_and_unseen() {
        // Below threshold: nothing to pop.
        assert!(next_earned_pop(TipSurface::Tui, &[], &signals(0)).is_none());

        // At threshold: the new-from-selection tip pops.
        let pop = next_earned_pop(
            TipSurface::Tui,
            &[],
            &signals(NEW_FROM_SELECTION_TIP_THRESHOLD),
        );
        assert_eq!(pop.map(|t| t.id), Some("new-from-selection"));

        // Once seen, it no longer pops even when eligible.
        let seen = vec!["new-from-selection".to_string()];
        assert!(next_earned_pop(
            TipSurface::Tui,
            &seen,
            &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)
        )
        .is_none());
    }

    #[test]
    fn every_tip_lists_at_least_one_surface() {
        for tip in catalog() {
            assert!(!tip.surfaces.is_empty(), "{} lists no surface", tip.id);
        }
    }

    #[test]
    fn surfaces_do_not_leak_across() {
        // The keyboard-shortcut tip is TUI-only; the PWA tip is web-only. Each
        // surface sees its own and never the other's, even when eligible.
        let earned = signals(NEW_FROM_SELECTION_TIP_THRESHOLD);

        let web = eligible(TipSurface::Web, &earned);
        assert!(web.iter().any(|t| t.id == "install-dashboard-pwa"));
        assert!(!web.iter().any(|t| t.id == "new-from-selection"));

        let tui = eligible(TipSurface::Tui, &earned);
        assert!(tui.iter().any(|t| t.id == "new-from-selection"));
        assert!(!tui.iter().any(|t| t.id == "install-dashboard-pwa"));
    }

    #[test]
    fn web_rotation_tips_are_eligible_by_default() {
        // Web tips are all rotation, so a brand-new web user sees every one of
        // them with no signals, and seeing one drops it from the count.
        let all = web_unseen_ids(&[], &signals(0));
        assert!(all.contains(&"install-dashboard-pwa"));
        assert!(all.len() > 1, "more than just the PWA tip ships on the web");
        let seen = vec!["install-dashboard-pwa".to_string()];
        assert_eq!(web_unseen_ids(&seen, &signals(0)).len(), all.len() - 1);
        // No web tip is earned, so nothing pops on its own.
        assert!(next_earned_pop(TipSurface::Web, &[], &signals(0)).is_none());
    }

    #[test]
    fn web_tips_carry_no_keybinding_placeholders() {
        // Web bodies are rendered as-is by the server (no placeholder resolver),
        // so a `{...}` would leak raw into the dashboard.
        for tip in catalog() {
            if tip.surfaces.contains(&TipSurface::Web) {
                assert!(!tip.body.contains('{'), "{} has a placeholder", tip.id);
            }
        }
    }

    #[test]
    fn id_in_catalog_matches_known_ids_only() {
        assert!(id_in_catalog("new-from-selection"));
        assert!(id_in_catalog("install-dashboard-pwa"));
        assert!(!id_in_catalog("nope"));
        assert!(!id_in_catalog(""));
    }
}
