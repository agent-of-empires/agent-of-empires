//! User-facing triage state: archive, trash, favorite, pin, snooze, unread,
//! color, and the idle bookkeeping they read.

use super::*;

/// The MVP palette for the per-session color label. Kept deliberately small and status-oriented.
pub const SESSION_COLORS: &[&str] = &["red", "amber", "green"];

/// True when `color` is a member of the [`SESSION_COLORS`] palette.
pub fn is_valid_session_color(color: &str) -> bool {
    SESSION_COLORS.contains(&color)
}

/// Mutually-exclusive lifecycle bucket a session belongs to, computed by
/// `Instance::effective_bucket()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBucket {
    Active,
    Archived,
    Trashed,
}

impl Instance {
    /// Stamp `last_accessed_at` to the current time AND wake the session from any sink state.
    pub fn touch_last_accessed(&mut self) {
        self.last_accessed_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
        self.idle_dormant_since = None;
    }

    /// Whether this session's structured view worker was auto-stopped for inactivity and should not
    /// be respawned by the reconciler until the user wakes it.
    pub fn is_idle_dormant(&self) -> bool {
        self.idle_dormant_since.is_some()
    }

    /// Mark the session dormant after its structured view worker was auto-stopped
    /// for inactivity. Idempotent: re-marking refreshes the timestamp.
    pub fn mark_idle_dormant(&mut self) {
        self.idle_dormant_since = Some(Utc::now());
    }

    /// Whether this session should render as "dormant" (worker auto-stopped for inactivity,
    /// resumable) rather than with its raw `status`.
    pub fn is_shown_dormant(&self) -> bool {
        self.is_idle_dormant() && self.status != Status::Stopped
    }

    /// Mark the session archived. Archived sessions sink to the bottom of the Attention sort and
    /// render in italic+dim style, but remain visible.
    pub fn archive(&mut self) {
        self.archived_at = Some(Utc::now());
        self.favorited_at = None;
        self.snoozed_until = None;
        self.pinned_at = None;
        self.settle_archived_status();
    }

    /// Idle is the resting state an archived row can truthfully claim; see `archive`.
    pub(crate) fn settle_archived_status(&mut self) {
        if matches!(
            self.status,
            Status::Running | Status::Waiting | Status::Starting
        ) {
            self.status = Status::Idle;
        }
    }

    pub fn unarchive(&mut self) {
        self.archived_at = None;
        self.idle_dormant_since = None;
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// Soft-delete the session into the trash bucket. Stops the live session (handled by the
    /// caller.
    pub fn trash(&mut self) {
        if self.trashed_at.is_none() {
            self.trashed_at = Some(Utc::now());
        }
    }

    /// Restore a trashed session back to its prior bucket (active or
    /// archived, depending on the preserved sibling flags). Idempotent.
    pub fn untrash(&mut self) {
        self.trashed_at = None;
    }

    pub fn is_trashed(&self) -> bool {
        self.trashed_at.is_some()
    }

    /// The mutually-exclusive lifecycle bucket a session renders in. Precedence is `Trashed >
    /// Archived > Active`.
    pub fn effective_bucket(&self) -> SessionBucket {
        if self.is_trashed() {
            SessionBucket::Trashed
        } else if self.is_archived() {
            SessionBucket::Archived
        } else {
            SessionBucket::Active
        }
    }

    /// Mark the session favorite. Sibling of `archive`, with opposite semantics.
    pub fn favorite(&mut self) {
        self.favorited_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
    }

    pub fn unfavorite(&mut self) {
        self.favorited_at = None;
    }

    pub fn is_favorited(&self) -> bool {
        self.favorited_at.is_some()
    }

    /// Set (or clear, with `None`) the per-session color label. Only a value in the
    /// [`SESSION_COLORS`] palette is accepted.
    pub fn set_color(&mut self, color: Option<String>) -> Result<(), String> {
        match color {
            None => self.color = None,
            Some(c) => {
                if !is_valid_session_color(&c) {
                    return Err(format!(
                        "invalid color {:?}; expected one of: {}, or none",
                        c,
                        SESSION_COLORS.join(", ")
                    ));
                }
                self.color = Some(c);
            }
        }
        Ok(())
    }

    /// Read the agent-raised urgent flag from `attention.json`.
    pub fn is_urgent(&self) -> bool {
        if self.is_archived() || self.is_snoozed() {
            return false;
        }
        crate::hooks::read_hook_urgent(&self.id)
    }

    /// Temporarily defer this session for `minutes`; sets `snoozed_until` to `Utc::now() +
    /// minutes`.
    pub fn snooze(&mut self, minutes: u32) {
        self.snoozed_until = Some(Utc::now() + chrono::Duration::minutes(minutes as i64));
        self.pinned_at = None;
    }

    pub fn unsnooze(&mut self) {
        self.snoozed_until = None;
    }

    /// True if the session carries the unread marker.
    pub fn is_unread(&self) -> bool {
        self.unread
    }

    /// Mark the session unread. Used both by the auto-mark on a finished turn (`Running -> Idle`)
    /// and the manual "Mark as unread" action.
    pub fn mark_unread(&mut self) {
        self.unread = true;
    }

    /// Clear the unread marker. Used whenever the user engages with the session (open/attach,
    /// live-send, click, dwell) and by the explicit "Mark as read" action.
    pub fn mark_read(&mut self) {
        self.unread = false;
    }

    /// Manual toggle (`U`): read -> unread; unread -> read.
    pub fn toggle_unread(&mut self) {
        self.unread = !self.unread;
    }

    /// True if `snoozed_until` is set AND in the future. Expired snoozes return false so the row
    /// naturally rejoins the main sort on the next render.
    pub fn is_snoozed(&self) -> bool {
        self.snoozed_until.map(|t| t > Utc::now()).unwrap_or(false)
    }

    /// Combined "don't bother me" sink-state check: trashed, snoozed, or archived.
    pub fn is_dismissed(&self) -> bool {
        self.is_trashed() || self.is_snoozed() || self.is_archived()
    }

    /// Remaining snooze duration as a `chrono::Duration`, or `None` if the
    /// session isn't snoozed (or the timestamp has already expired).
    pub fn snooze_remaining(&self) -> Option<chrono::Duration> {
        self.snoozed_until.and_then(|t| {
            let delta = t - Utc::now();
            if delta > chrono::Duration::zero() {
                Some(delta)
            } else {
                None
            }
        })
    }

    /// Mark this session pinned. Pin is a web-only surfacing primitive.
    pub fn pin(&mut self) {
        self.pinned_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
    }

    pub fn unpin(&mut self) {
        self.pinned_at = None;
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned_at.is_some()
    }

    /// Time elapsed since this session most recently transitioned into `Idle`.
    pub fn idle_age(&self) -> Option<std::time::Duration> {
        if self.status != Status::Idle {
            return None;
        }
        let since = self.idle_entered_at?;
        (Utc::now() - since).to_std().ok()
    }

    /// True iff this session should keep the machine awake: it is active (`Running`, `Waiting`,
    /// `Starting`, or `Creating`), or it went idle less than `window` ago.
    pub fn has_recent_activity(&self, window: std::time::Duration) -> bool {
        matches!(
            self.status,
            Status::Running | Status::Waiting | Status::Starting | Status::Creating
        ) || matches!(self.idle_age(), Some(age) if age < window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> Instance {
        Instance::new("test", "/tmp/test")
    }

    #[test]
    fn set_color_accepts_only_the_palette() {
        let mut inst = inst();
        for c in SESSION_COLORS {
            inst.set_color(Some((*c).to_string())).unwrap();
            assert_eq!(inst.color.as_deref(), Some(*c));
        }
        inst.set_color(None).unwrap();
        assert_eq!(inst.color, None);

        inst.set_color(Some("green".to_string())).unwrap();
        let err = inst.set_color(Some("chartreuse".to_string())).unwrap_err();
        assert!(err.contains("chartreuse"), "{err}");
        assert_eq!(inst.color.as_deref(), Some("green"));

        for (color, valid) in [
            ("red", true),
            ("amber", true),
            ("green", true),
            ("blue", false),
            ("", false),
            ("Red", false),
        ] {
            assert_eq!(is_valid_session_color(color), valid, "{color}");
        }
    }

    #[test]
    fn triage_mutators_keep_their_exclusivity_rules() {
        type Action = fn(&mut Instance);
        type Check = fn(&Instance) -> bool;
        let archived: Check = Instance::is_archived;
        let snoozed: Check = Instance::is_snoozed;
        let dormant: Check = Instance::is_idle_dormant;
        let favorited: Check = Instance::is_favorited;
        let pinned: Check = Instance::is_pinned;
        let touch: Action = Instance::touch_last_accessed;
        // (label, setup, action, [(check, expected after action)])
        let cases: &[(&str, &[Action], Action, &[(Check, bool)])] = &[
            (
                "touch wakes archive",
                &[|i| i.archive()],
                touch,
                &[(archived, false)],
            ),
            (
                "touch wakes snooze",
                &[|i| i.snooze(30)],
                touch,
                &[(snoozed, false)],
            ),
            (
                "touch wakes dormancy",
                &[|i| i.mark_idle_dormant()],
                touch,
                &[(dormant, false)],
            ),
            (
                "touch keeps favorite",
                &[|i| i.favorite()],
                touch,
                &[(favorited, true)],
            ),
            ("touch keeps pin", &[|i| i.pin()], touch, &[(pinned, true)]),
            (
                "unarchive wakes dormancy",
                &[|i| i.archive(), |i| i.mark_idle_dormant()],
                |i| i.unarchive(),
                &[(archived, false), (dormant, false)],
            ),
            (
                "archive clears snooze",
                &[|i| i.snooze(15)],
                |i| i.archive(),
                &[(archived, true), (snoozed, false)],
            ),
            (
                "archive clears pin",
                &[|i| i.pin()],
                |i| i.archive(),
                &[(archived, true), (pinned, false)],
            ),
            (
                "pin clears archive",
                &[|i| i.archive()],
                |i| i.pin(),
                &[(pinned, true), (archived, false), (snoozed, false)],
            ),
            (
                "pin clears snooze",
                &[|i| i.snooze(15)],
                |i| i.pin(),
                &[(pinned, true), (snoozed, false)],
            ),
            (
                "snooze clears pin",
                &[|i| i.pin()],
                |i| i.snooze(30),
                &[(snoozed, true), (pinned, false)],
            ),
            (
                "pin keeps favorite",
                &[|i| i.favorite()],
                |i| i.pin(),
                &[(pinned, true), (favorited, true)],
            ),
            (
                "favorite keeps pin",
                &[|i| i.pin()],
                |i| i.favorite(),
                &[(pinned, true), (favorited, true)],
            ),
            (
                "mark dormant",
                &[],
                |i| i.mark_idle_dormant(),
                &[(dormant, true)],
            ),
        ];
        for (label, setup, action, checks) in cases {
            let mut inst = inst();
            setup.iter().for_each(|step| step(&mut inst));
            action(&mut inst);
            for (check, expected) in checks.iter() {
                assert_eq!(check(&inst), *expected, "{label}");
            }
        }
        let mut touched = inst();
        touched.touch_last_accessed();
        assert!(touched.last_accessed_at.is_some());
    }

    #[test]
    fn unread_marker_is_idempotent_toggles_and_skips_serialization_when_false() {
        let mut inst = inst();
        assert!(!inst.is_unread());
        for (step, expected) in [
            (Instance::mark_unread as fn(&mut Instance), true),
            (Instance::mark_unread, true),
            (Instance::mark_read, false),
            (Instance::mark_read, false),
            (Instance::toggle_unread, true),
            (Instance::toggle_unread, false),
        ] {
            step(&mut inst);
            assert_eq!(inst.is_unread(), expected);
        }
        assert!(serde_json::to_value(&inst).unwrap().get("unread").is_none());
        inst.unread = true;
        let json = serde_json::to_value(&inst).unwrap();
        assert_eq!(json["unread"], serde_json::json!(true));
        assert!(serde_json::from_value::<Instance>(json).unwrap().unread);
    }

    #[test]
    fn dormancy_presents_only_on_an_idle_row() {
        for (status, marked, shown) in [
            (Status::Idle, true, true),
            // A deliberate Stop also marks dormant but presents as stopped.
            (Status::Stopped, true, false),
            (Status::Idle, false, false),
            (Status::Running, false, false),
        ] {
            let mut inst = inst();
            inst.status = status;
            if marked {
                inst.mark_idle_dormant();
            }
            assert_eq!(inst.is_shown_dormant(), shown, "{status:?} {marked}");
        }
    }

    #[test]
    fn trash_wins_the_bucket_and_preserves_decorations() {
        let mut inst = inst();
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
        let json = serde_json::to_string(&inst).unwrap();
        assert!(!json.contains("trashed_at"));
        inst.favorite();
        inst.pin();
        inst.trash();
        assert!(inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Trashed);
        assert!(inst.is_favorited() && inst.is_pinned());
        let back: Instance = serde_json::from_str(&serde_json::to_string(&inst).unwrap()).unwrap();
        assert!(back.is_trashed());
        inst.untrash();
        assert!(!inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
        assert!(inst.is_favorited() && inst.is_pinned());

        let mut archived = self::inst();
        archived.archive();
        assert_eq!(archived.effective_bucket(), SessionBucket::Archived);
        archived.trash();
        assert_eq!(archived.effective_bucket(), SessionBucket::Trashed);
        archived.untrash();
        assert_eq!(archived.effective_bucket(), SessionBucket::Archived);
    }

    #[test]
    fn idle_age_and_recent_activity() {
        let window = std::time::Duration::from_secs(15 * 60);
        let ago = |secs: i64| Some(Utc::now() - chrono::Duration::seconds(secs));
        // (status, idle_entered_at, idle age present, recent activity)
        for (status, entered, has_age, recent) in [
            (Status::Running, ago(60), false, Some(true)),
            (Status::Waiting, None, false, Some(true)),
            (Status::Starting, None, false, Some(true)),
            (Status::Creating, None, false, Some(true)),
            (Status::Stopped, None, false, Some(false)),
            (Status::Error, None, false, Some(false)),
            (Status::Unknown, None, false, Some(false)),
            (Status::Deleting, None, false, Some(false)),
            (Status::Idle, None, false, Some(false)),
            (Status::Idle, ago(60), true, Some(true)),
            (Status::Idle, ago(30 * 60), true, Some(false)),
            // A future timestamp (clock skew) clamps to no age.
            (Status::Idle, ago(-60), false, None),
        ] {
            let mut inst = inst();
            inst.status = status;
            inst.idle_entered_at = entered;
            assert_eq!(inst.idle_age().is_some(), has_age, "{status:?} {entered:?}");
            if let Some(recent) = recent {
                assert_eq!(
                    inst.has_recent_activity(window),
                    recent,
                    "{status:?} {entered:?}"
                );
            }
        }
        let mut inst = inst();
        inst.status = Status::Idle;
        inst.idle_entered_at = ago(5);
        let age = inst.idle_age().unwrap().as_secs();
        assert!((4..=30).contains(&age));
    }

    #[test]
    fn archive_settles_only_live_interaction_statuses() {
        for (status, expected) in [
            (Status::Running, Status::Idle),
            (Status::Waiting, Status::Idle),
            (Status::Starting, Status::Idle),
            (Status::Idle, Status::Idle),
            (Status::Stopped, Status::Stopped),
            (Status::Error, Status::Error),
            (Status::Unknown, Status::Unknown),
        ] {
            let mut inst = inst();
            inst.status = status;
            inst.archive();
            assert!(inst.is_archived());
            assert_eq!(inst.status, expected, "{status:?}");
        }
    }
}
