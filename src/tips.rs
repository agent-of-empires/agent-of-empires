//! Tips catalog and selection logic, shared by every surface. Rotation tips are always
//! eligible; earned tips need a behavior signal and may pop once.

#[derive(Debug, Clone, Default)]
pub struct TipSignals {
    pub new_session_with_selection_count: u32,
    pub used_new_from_selection: bool,
    pub system_health_tip_earned: bool,
    pub used_system_health: bool,
}

pub const NEW_FROM_SELECTION_TIP_THRESHOLD: u32 = 3;
pub const SYSTEM_HEALTH_AGENT_THRESHOLD: usize = 6;
pub const SYSTEM_HEALTH_SAMPLE_THRESHOLD: u8 = 3;

/// A tip shows only on the surfaces it lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipSurface {
    Tui,
    Web,
}

pub enum TipTrigger {
    Rotation,
    Earned(fn(&TipSignals) -> bool),
}

pub struct Tip {
    /// Persistence key in `tips_seen`: never reuse or renumber.
    pub id: &'static str,
    pub title: &'static str,
    pub body: &'static str,
    pub trigger: TipTrigger,
    pub surfaces: &'static [TipSurface],
}

impl Tip {
    fn is_eligible(&self, surface: TipSurface, signals: &TipSignals) -> bool {
        self.surfaces.contains(&surface)
            && match self.trigger {
                TipTrigger::Rotation => true,
                TipTrigger::Earned(predicate) => predicate(signals),
            }
    }

    pub fn is_earned(&self) -> bool {
        matches!(self.trigger, TipTrigger::Earned(_))
    }
}

fn earned_new_from_selection(signals: &TipSignals) -> bool {
    !signals.used_new_from_selection
        && signals.new_session_with_selection_count >= NEW_FROM_SELECTION_TIP_THRESHOLD
}

fn earned_system_health(signals: &TipSignals) -> bool {
    signals.system_health_tip_earned && !signals.used_system_health
}

pub fn catalog() -> &'static [Tip] {
    CATALOG
}

static CATALOG: &[Tip] = &[
    Tip {
        id: "new-from-selection",
        title: "Reuse the selected session's settings",
        // `{placeholder}` keys are substituted with the live chord by the tips overlay.
        body: "Tired of choosing the directory, profile, and group every time? Press \
               {new_from_selection} on the home view to start a new session that inherits \
               all of them from the session you have selected.",
        trigger: TipTrigger::Earned(earned_new_from_selection),
        surfaces: &[TipSurface::Tui],
    },
    Tip {
        id: "system-health",
        title: "See what your agents are costing",
        body: "You have 6 or more agents running. Turn on the System Health strip in \
               Settings, or open System Health from the command palette, to see CPU, \
               memory, load, swap, and per-agent usage.",
        trigger: TipTrigger::Earned(earned_system_health),
        surfaces: &[TipSurface::Tui],
    },
    Tip {
        id: "install-dashboard-pwa",
        title: "Install the dashboard as an app",
        body: "You can install the dashboard as an app for quick access. In your browser, \
               use the install option (Install Agent of Empires in Chrome, or Add to Home \
               Screen on iOS) to keep it one tap away and keep notifications working.",
        trigger: TipTrigger::Rotation,
        surfaces: &[TipSurface::Web],
    },
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
        body: "Use the grouping toggle in the sidebar to switch between By repo, By org, By \
               group, and By repo and group. Right-click a session and choose Edit group to \
               file it under any name you like.",
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

pub fn id_in_catalog(id: &str) -> bool {
    catalog().iter().any(|tip| tip.id == id)
}

fn is_seen(seen: &[String], id: &str) -> bool {
    seen.iter().any(|s| s == id)
}

pub fn eligible(surface: TipSurface, signals: &TipSignals) -> Vec<&'static Tip> {
    catalog()
        .iter()
        .filter(|tip| tip.is_eligible(surface, signals))
        .collect()
}

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

pub fn unseen_count(surface: TipSurface, seen: &[String], signals: &TipSignals) -> usize {
    eligible_unseen(surface, seen, signals).len()
}

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
            ..TipSignals::default()
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
        let used = TipSignals {
            new_session_with_selection_count: NEW_FROM_SELECTION_TIP_THRESHOLD + 5,
            used_new_from_selection: true,
            ..TipSignals::default()
        };
        assert!(!tip.is_eligible(TipSurface::Tui, &used));
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
        let base = unseen_count(TipSurface::Tui, &[], &signals(0));
        assert_eq!(
            unseen_count(
                TipSurface::Tui,
                &[],
                &signals(NEW_FROM_SELECTION_TIP_THRESHOLD)
            ),
            base + 1
        );

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
    fn system_health_tip_requires_earned_and_undiscovered() {
        let tip = by_id("system-health").unwrap();
        let cases = [
            (false, false, false),
            (true, false, true),
            (true, true, false),
        ];
        for (earned, used, expected) in cases {
            let signals = TipSignals {
                system_health_tip_earned: earned,
                used_system_health: used,
                ..TipSignals::default()
            };
            assert_eq!(tip.is_eligible(TipSurface::Tui, &signals), expected);
        }
    }

    #[test]
    fn next_earned_pop_only_when_eligible_and_unseen() {
        assert!(next_earned_pop(TipSurface::Tui, &[], &signals(0)).is_none());

        let pop = next_earned_pop(
            TipSurface::Tui,
            &[],
            &signals(NEW_FROM_SELECTION_TIP_THRESHOLD),
        );
        assert_eq!(pop.map(|t| t.id), Some("new-from-selection"));

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
        let all = web_unseen_ids(&[], &signals(0));
        assert!(all.contains(&"install-dashboard-pwa"));
        assert!(all.len() > 1, "more than just the PWA tip ships on the web");
        let seen = vec!["install-dashboard-pwa".to_string()];
        assert_eq!(web_unseen_ids(&seen, &signals(0)).len(), all.len() - 1);
        assert!(next_earned_pop(TipSurface::Web, &[], &signals(0)).is_none());
    }

    #[test]
    fn web_tips_carry_no_keybinding_placeholders() {
        // Web bodies render as-is, so a `{...}` placeholder would leak into the dashboard.
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
