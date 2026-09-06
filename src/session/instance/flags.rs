//! User-facing triage state: archive, trash, favorite, pin, snooze, unread,
//! color, and the idle bookkeeping they read.

use super::*;

/// The MVP palette for the per-session color label (#2383). Kept deliberately
/// small and status-oriented: red = needs attention / blocked, amber =
/// working / in progress, green = done / ready. `None`/absent clears the dot.
/// Both the CLI (`aoe session color`) and the web PATCH endpoint validate
/// against this list via [`is_valid_session_color`].
pub const SESSION_COLORS: &[&str] = &["red", "amber", "green"];

/// True when `color` is a member of the [`SESSION_COLORS`] palette.
pub fn is_valid_session_color(color: &str) -> bool {
    SESSION_COLORS.contains(&color)
}

/// Mutually-exclusive lifecycle bucket a session belongs to, computed by
/// `Instance::effective_bucket()`. Precedence is `Trashed > Archived >
/// Active`. Used to route a session into the right list (active sidebar,
/// archived fold, or trash view) and to filter the `GET /api/sessions`
/// response by `?state=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBucket {
    Active,
    Archived,
    Trashed,
}

impl Instance {
    /// Stamp `last_accessed_at` to the current time AND wake the session
    /// from any sink state. Call this on user-initiated interactions
    /// (attach, send keys, etc.); every existing call site already does.
    ///
    /// Auto-unarchive/unsnooze: sending a message or attaching is the user
    /// explicitly saying "I care about this now." Leaving `archived_at` or
    /// `snoozed_until` set after such interaction is incoherent; the row
    /// would render italic+dim at tier 99 even while live traffic flows.
    /// User rule (2026-04-23): "messaging should unarchive."
    ///
    /// `favorited_at` is preserved: fav is a positive "care more" signal,
    /// orthogonal to the sink states. A favorited session that was snoozed
    /// stays favorited when the user wakes it.
    pub fn touch_last_accessed(&mut self) {
        self.last_accessed_at = Some(Utc::now());
        self.archived_at = None;
        self.snoozed_until = None;
        self.idle_dormant_since = None;
    }

    /// Whether this session's structured view worker was auto-stopped for
    /// inactivity and should not be respawned by the reconciler until the
    /// user wakes it. See `idle_dormant_since` and #1689.
    pub fn is_idle_dormant(&self) -> bool {
        self.idle_dormant_since.is_some()
    }

    /// Mark the session dormant after its structured view worker was auto-stopped
    /// for inactivity. Idempotent: re-marking refreshes the timestamp.
    pub fn mark_idle_dormant(&mut self) {
        self.idle_dormant_since = Some(Utc::now());
    }

    /// Whether this session should render as "dormant" (worker auto-stopped
    /// for inactivity, resumable) rather than with its raw `status`. This is
    /// the single source of the deliberate-stop-vs-dormant precedence: a
    /// deliberate Stop also sets `idle_dormant_since` (see `stop_session`),
    /// so `Status::Stopped` must win here and keep showing the neutral
    /// "Stopped" dot; only a non-stopped row carrying the dormant marker
    /// (the idle-reaper's output) presents as dormant. The reaper only ever
    /// marks structured rows, so this is structured-only in practice. See
    /// #2250 and `idle_dormant_since`.
    pub fn is_shown_dormant(&self) -> bool {
        self.is_idle_dormant() && self.status != Status::Stopped
    }

    /// Mark the session archived. Archived sessions sink to the bottom of
    /// the Attention sort and render in italic+dim style, but remain visible.
    /// Archive suppresses the attention signal rather than the signal
    /// clearing archive: `is_urgent` returns false while archived, and the
    /// attention sort short-circuits the row to its bottom tier.
    ///
    /// Cleared by `unarchive`, by `touch_last_accessed`, and by `favorite`
    /// and `pin`; not by `snooze`.
    /// `merge_user_action_diff` mirrors those onto disk; #3465 was a status
    /// transition reaching that mirror without a user gesture.
    ///
    /// Mutual exclusion with `favorite`, `snooze`, and `pin`: archiving
    /// clears `favorited_at`, `snoozed_until`, and `pinned_at`. Archive
    /// is the strongest dismiss; keeping any other triage flag on a row
    /// the user just sunk produces contradictory state, and the web
    /// sidebar's tier comparator already assumes the server enforces a
    /// single active triage state (see `sidebarSort.ts` in #1581).
    ///
    /// Archiving tears down the session's tmux (#1868), so a live-interaction
    /// status (Running/Waiting/Starting) cannot be true of an archived row.
    /// Left in place, a frozen `Waiting` keeps rendering as a
    /// pending-permission row forever — the status poller deliberately never
    /// touches archived rows (#2206), so nothing else can clear it. Degrade
    /// those statuses to Idle here, matching where v016 settles archived rows.
    pub fn archive(&mut self) {
        self.archived_at = Some(Utc::now());
        self.favorited_at = None;
        self.snoozed_until = None;
        self.pinned_at = None;
        self.settle_archived_status();
    }

    /// Idle is the resting state an archived row can truthfully claim; see
    /// `archive`. Shared with the status poller's archived short-circuit (so
    /// a row frozen by an older build heals in memory without waiting for the
    /// one-shot v028 migration) and with the three disk-write merges that can
    /// land a status or an archive on a row (`merge_user_action_diff`,
    /// `merge_passive_status_patch`, `merge_from_tui`), so a stale
    /// pre-archive observation cannot re-freeze it.
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

    /// Soft-delete the session into the trash bucket. Stops the live
    /// session (handled by the caller: ACP `shutdown`, optional tmux kill)
    /// but keeps every durable artifact so `untrash` can bring it back
    /// intact. Intentionally additive: only `trashed_at` is set, the
    /// sibling triage flags (`archived_at`, `favorited_at`, `snoozed_until`,
    /// `pinned_at`) are left untouched so restore is faithful.
    /// `effective_bucket()` makes trash win regardless. Idempotent.
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

    /// The mutually-exclusive lifecycle bucket a session renders in.
    /// Precedence is `Trashed > Archived > Active`: a trashed row never
    /// shows in active or archived views, and an archived row never shows
    /// in active views. Snooze/favorite/pin are orthogonal decorations
    /// within a bucket, not buckets of their own, so they are not consulted
    /// here. Use this instead of bare `!is_archived()` filters so trashed
    /// rows cannot leak into the active list.
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
    /// Pinning logic lives in `attention_session_key`: favorite is a
    /// within-tier pin (top of its respective category), not a cross-tier
    /// promoter. A favorited Running stays in the Running bucket but sorts
    /// above non-favorited Running peers.
    ///
    /// Mutual exclusion with the sink states: favoriting clears `archived_at`
    /// AND `snoozed_until`. Favorite's whole purpose is "surface this row";
    /// leaving either sink-state flag set would force the row to tier 99 and
    /// the favorite bias would be suppressed; user presses `f` and sees
    /// nothing change. The user's explicit rule: "marking as favorite
    /// unarchives," extended to snooze because snooze shares tier 99 and
    /// shares the burial outcome.
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

    /// Set (or clear, with `None`) the per-session color label. Only a value
    /// in the [`SESSION_COLORS`] palette is accepted; anything else is
    /// rejected so the sidebar never has to render an unknown swatch. See
    /// #2383.
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

    /// Read the agent-raised urgent flag from `attention.json`. Sourced
    /// on-demand from `/tmp/aoe-hooks-<euid>/{id}/attention.json` so it picks up
    /// changes the running agent makes (via the `attention-urgent` script)
    /// without an Instance state mutation. Suppressed for archived/snoozed
    /// rows so a sunk session can't claw its way back to the top.
    pub fn is_urgent(&self) -> bool {
        if self.is_archived() || self.is_snoozed() {
            return false;
        }
        crate::hooks::read_hook_urgent(&self.id)
    }

    /// Temporarily defer this session for `minutes`; sets `snoozed_until`
    /// to `Utc::now() + minutes`. Behaves like a timed archive: the row
    /// sinks to tier 99, renders italic+dim with a `z ` prefix, and shows
    /// remaining time in the age column. When the timestamp expires the
    /// row rejoins the active attention sort automatically (next render
    /// tick); no timer task needed. Resolution of `minutes` happens at
    /// snooze time, not render time, so changing the config default mid-
    /// snooze does NOT extend currently-sleeping rows.
    ///
    /// Clears `pinned_at` for the same reason archive does: snooze is a
    /// sink state, and a pinned-yet-snoozed row is contradictory. The
    /// existing favorite mutator is intentionally NOT touched here
    /// (favorite is the TUI within-tier signal, snoozed favorites keep
    /// their star when they wake; see field doc for `favorited_at`).
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

    /// Mark the session unread. Used both by the auto-mark on a finished turn
    /// (`Running -> Idle`) and the manual "Mark as unread" action; the single
    /// state means there is no kind to preserve. Idempotent.
    pub fn mark_unread(&mut self) {
        self.unread = true;
    }

    /// Clear the unread marker. Used whenever the user engages with the
    /// session (open/attach, live-send, click, dwell) and by the explicit
    /// "Mark as read" action. Idempotent.
    pub fn mark_read(&mut self) {
        self.unread = false;
    }

    /// Manual toggle (`U`): read -> unread; unread -> read.
    pub fn toggle_unread(&mut self) {
        self.unread = !self.unread;
    }

    /// True if `snoozed_until` is set AND in the future. Expired snoozes
    /// return false so the row naturally rejoins the main sort on the next
    /// render; the stale timestamp stays on disk until the next mutation
    /// rewrites the session (harmless; `snoozed_until` is always compared
    /// against `Utc::now()`).
    pub fn is_snoozed(&self) -> bool {
        self.snoozed_until.map(|t| t > Utc::now()).unwrap_or(false)
    }

    /// Combined "don't bother me" sink-state check: trashed, snoozed, or
    /// archived. Callers that walk sessions looking for something to land on
    /// (e.g. the `w`/jump-to-next-attention passes) use this instead of the
    /// three-call form so a row in any sink state is uniformly excluded.
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

    /// Mark this session pinned. Pin is a web-only surfacing primitive:
    /// pinned workspaces sort to the top of the web sidebar (across all
    /// sort modes), regardless of last-activity. Distinct from
    /// `favorited_at`, which drives the TUI Attention sort's within-tier
    /// pin and stays unchanged here (see #1581).
    ///
    /// Mutual exclusion with the sink states: pinning clears
    /// `archived_at` and `snoozed_until`. A pinned-yet-sunk row would
    /// contradict the entire point of pinning (surface this), so the
    /// sinks come off, identical to how `favorite()` handles it.
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

    /// Time elapsed since this session most recently transitioned into
    /// `Idle`. `None` for non-Idle sessions, sessions with a missing
    /// timestamp (legacy state), or sessions whose `idle_entered_at` is in
    /// the future (clock skew). Negative deltas are clamped away rather than
    /// returned as `Duration` since `chrono::Duration::to_std` rejects them.
    pub fn idle_age(&self) -> Option<std::time::Duration> {
        if self.status != Status::Idle {
            return None;
        }
        let since = self.idle_entered_at?;
        (Utc::now() - since).to_std().ok()
    }

    /// True iff this session should keep the machine awake: it is active
    /// (`Running`, `Waiting`, `Starting`, or `Creating`), or it went idle less
    /// than `window` ago. A session idle for `>= window` (or
    /// Stopped/Error/Unknown/Deleting) returns false, so the sleep-inhibit
    /// assertion may release. `Waiting`, `Starting`, and `Creating` all count
    /// as active unconditionally, so a session parked waiting for input, or
    /// one still starting or mid-create, holds sleep until it leaves that
    /// status: the predicate ages out only `Idle`, never these three. That is
    /// intentional for the opt-in v1, and nothing ages these three out:
    /// `Waiting` (an unanswered prompt) and `Creating` (a container, worktree,
    /// or submodule setup that never returns) can hold sleep indefinitely,
    /// while `Starting` is bounded by the ~3s `last_start_time` guard in
    /// `update_status_with_metadata_inner` and then re-resolves.
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

    #[test]
    fn set_color_accepts_palette_and_clears_with_none() {
        let mut inst = Instance::new("color-test", "/tmp");
        assert_eq!(inst.color, None);

        for c in SESSION_COLORS {
            inst.set_color(Some((*c).to_string())).unwrap();
            assert_eq!(inst.color.as_deref(), Some(*c));
        }

        inst.set_color(None).unwrap();
        assert_eq!(inst.color, None);
    }

    #[test]
    fn set_color_rejects_unknown_color_and_leaves_prior_value() {
        let mut inst = Instance::new("color-test", "/tmp");
        inst.set_color(Some("green".to_string())).unwrap();

        let err = inst
            .set_color(Some("chartreuse".to_string()))
            .expect_err("unknown color must be rejected");
        assert!(
            err.contains("chartreuse"),
            "error should name the value: {err}"
        );
        // A rejected write must not clobber the previously stored color.
        assert_eq!(inst.color.as_deref(), Some("green"));
    }

    #[test]
    fn is_valid_session_color_matches_palette() {
        assert!(is_valid_session_color("red"));
        assert!(is_valid_session_color("amber"));
        assert!(is_valid_session_color("green"));
        assert!(!is_valid_session_color("blue"));
        assert!(!is_valid_session_color(""));
        assert!(!is_valid_session_color("Red"));
    }

    /// `touch_last_accessed` is what `aoe send` and the TUI dispatch path
    /// call when the user interacts with a session. It must auto-wake
    /// archived and snoozed rows so sending a message to a sunk session
    /// brings it back, while preserving the favorite flag (favorite is a
    /// positive "care more" signal, not a sink state).
    #[test]
    fn test_touch_last_accessed_clears_archived() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        assert!(inst.is_archived());
        inst.touch_last_accessed();
        assert!(!inst.is_archived());
        assert!(inst.last_accessed_at.is_some());
    }

    #[test]
    fn test_touch_last_accessed_clears_snooze() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.snooze(30);
        assert!(inst.is_snoozed());
        inst.touch_last_accessed();
        assert!(!inst.is_snoozed());
    }

    #[test]
    fn test_touch_last_accessed_clears_idle_dormant() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.mark_idle_dormant();
        assert!(inst.is_idle_dormant());
        inst.touch_last_accessed();
        assert!(!inst.is_idle_dormant());
    }

    #[test]
    fn test_unarchive_clears_idle_dormant() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.mark_idle_dormant();
        assert!(inst.is_archived());
        assert!(inst.is_idle_dormant());

        inst.unarchive();

        assert!(!inst.is_archived());
        assert!(
            !inst.is_idle_dormant(),
            "unarchive should wake sessions blocked by idle auto-stop"
        );
    }

    #[test]
    fn test_mark_unread_and_mark_read_are_idempotent() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_unread());
        // read -> unread
        inst.mark_unread();
        assert!(inst.is_unread());
        // unread -> unread (idempotent)
        inst.mark_unread();
        assert!(inst.is_unread());
        // unread -> read
        inst.mark_read();
        assert!(!inst.is_unread());
        // read -> read (idempotent)
        inst.mark_read();
        assert!(!inst.is_unread());
    }

    #[test]
    fn test_toggle_unread_round_trips() {
        let mut inst = Instance::new("test", "/tmp/test");
        // read -> unread
        inst.toggle_unread();
        assert!(inst.is_unread());
        // unread -> read
        inst.toggle_unread();
        assert!(!inst.is_unread());
    }

    #[test]
    fn test_unread_serde_round_trip() {
        // Absent field deserializes to false (older sessions.json).
        let inst: Instance = serde_json::from_value(serde_json::json!({
            "id": "abc",
            "title": "t",
            "project_path": "/tmp",
            "tool": "claude",
            "status": "idle",
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("deserialize without unread");
        assert!(!inst.unread);

        // Round-trips when set, and is omitted when false.
        let mut set = Instance::new("t", "/tmp");
        set.unread = true;
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["unread"], serde_json::json!(true));
        let back: Instance = serde_json::from_value(json).unwrap();
        assert!(back.unread);

        let read = Instance::new("t", "/tmp");
        let json = serde_json::to_value(&read).unwrap();
        assert!(
            json.get("unread").is_none(),
            "false must skip serialization"
        );
    }

    #[test]
    fn test_mark_idle_dormant_sets_marker() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_idle_dormant());
        inst.mark_idle_dormant();
        assert!(inst.is_idle_dormant());
        assert!(inst.idle_dormant_since.is_some());
    }

    #[test]
    fn test_is_shown_dormant_precedence() {
        // Idle + dormant marker: the idle-reaper's output, presents dormant.
        let mut idle_reaped = Instance::new("test", "/tmp/test");
        idle_reaped.status = Status::Idle;
        idle_reaped.mark_idle_dormant();
        assert!(idle_reaped.is_shown_dormant());

        // Stopped + dormant marker: a deliberate Stop (which also marks
        // dormant). Stopped must win so the row keeps the neutral "Stopped"
        // dot, not the dormant one. See #2250.
        let mut deliberate_stop = Instance::new("test", "/tmp/test");
        deliberate_stop.status = Status::Stopped;
        deliberate_stop.mark_idle_dormant();
        assert!(!deliberate_stop.is_shown_dormant());

        // Idle, no marker: a live idle session, unaffected.
        let mut live_idle = Instance::new("test", "/tmp/test");
        live_idle.status = Status::Idle;
        assert!(!live_idle.is_shown_dormant());

        // Running, no marker: live, unaffected.
        let mut running = Instance::new("test", "/tmp/test");
        running.status = Status::Running;
        assert!(!running.is_shown_dormant());
    }

    #[test]
    fn test_touch_last_accessed_preserves_favorite() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.favorite();
        assert!(inst.is_favorited());
        inst.touch_last_accessed();
        // Favorite is orthogonal to sink states; user interaction must not
        // clear it.
        assert!(inst.is_favorited());
    }

    #[test]
    fn test_archive_clears_snooze() {
        // Direct mutator test (no merge): the data-layer contract is
        // that archive is mutually exclusive with every other triage
        // flag. The sidebar tier comparator in `sidebarSort.ts`
        // assumes the server enforces exactly one active state, so a
        // snooze-then-archive transition must leave only archive
        // behind. See #1581.
        let mut inst = Instance::new("s", "/tmp/x");
        inst.snooze(15);
        assert!(inst.is_snoozed());
        inst.archive();
        assert!(inst.is_archived());
        assert!(!inst.is_snoozed());
    }

    #[test]
    fn test_pin_clears_archive_and_snooze() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.archive();
        assert!(inst.is_archived());
        inst.pin();
        assert!(inst.is_pinned());
        assert!(!inst.is_archived());
        assert!(!inst.is_snoozed());

        inst.snooze(15);
        assert!(inst.is_snoozed());
        inst.pin();
        assert!(inst.is_pinned());
        assert!(!inst.is_snoozed());
    }

    #[test]
    fn test_archive_clears_pin() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.pin();
        assert!(inst.is_pinned());
        inst.archive();
        assert!(inst.is_archived());
        assert!(!inst.is_pinned());
    }

    #[test]
    fn test_trash_untrash_roundtrip() {
        let mut inst = Instance::new("s", "/tmp/x");
        assert!(!inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);

        inst.trash();
        assert!(inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Trashed);

        inst.untrash();
        assert!(!inst.is_trashed());
        assert_eq!(inst.effective_bucket(), SessionBucket::Active);
    }

    #[test]
    fn test_trash_preserves_sibling_triage_flags() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.favorite();
        inst.pin();
        assert!(inst.is_favorited());
        assert!(inst.is_pinned());

        inst.trash();
        // Trash wins the bucket but leaves the decorations intact so
        // restore is faithful (a trashed favorite comes back a favorite).
        assert_eq!(inst.effective_bucket(), SessionBucket::Trashed);
        assert!(inst.is_favorited(), "favorite preserved across trash");
        assert!(inst.is_pinned(), "pin preserved across trash");

        inst.untrash();
        assert!(inst.is_favorited());
        assert!(inst.is_pinned());
    }

    #[test]
    fn test_effective_bucket_trash_beats_archive() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.archive();
        assert_eq!(inst.effective_bucket(), SessionBucket::Archived);
        inst.trash();
        assert_eq!(
            inst.effective_bucket(),
            SessionBucket::Trashed,
            "trash takes precedence over archive in bucketing"
        );
        // archived_at is preserved, so restore returns to the archived bucket.
        assert!(inst.is_archived());
        inst.untrash();
        assert_eq!(inst.effective_bucket(), SessionBucket::Archived);
    }

    #[test]
    fn test_trashed_at_serde_roundtrip_and_default() {
        // A non-trashed instance omits trashed_at on the wire
        // (skip_serializing_if), so deserializing it exercises the
        // missing-field path that legacy rows hit: it must default to None,
        // which is why no migration is needed.
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(
            !fresh_json.contains("trashed_at"),
            "None trashed_at must not be serialized"
        );
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert!(!parsed.is_trashed(), "missing trashed_at => None");

        let mut inst = Instance::new("s", "/tmp/x");
        inst.trash();
        let json = serde_json::to_string(&inst).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert!(back.is_trashed());
    }

    #[test]
    fn test_snooze_clears_pin() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.pin();
        assert!(inst.is_pinned());
        inst.snooze(30);
        assert!(inst.is_snoozed());
        assert!(!inst.is_pinned());
    }

    #[test]
    fn test_touch_last_accessed_preserves_pin() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.pin();
        assert!(inst.is_pinned());
        inst.touch_last_accessed();
        // Pin is an explicit user surfacing signal, not a sink state.
        // User interaction (send, attach) must NOT clear it.
        assert!(inst.is_pinned());
    }

    #[test]
    fn test_pin_and_favorite_coexist() {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.favorite();
        assert!(inst.is_favorited());
        inst.pin();
        // Pin and favorite drive different surfaces (TUI Attention vs web
        // sidebar). They must coexist; pinning does NOT clear favorite.
        assert!(inst.is_pinned());
        assert!(inst.is_favorited());

        let mut inst2 = Instance::new("s2", "/tmp/x");
        inst2.pin();
        inst2.favorite();
        // Same in reverse: favoriting does NOT clear pin.
        assert!(inst2.is_pinned());
        assert!(inst2.is_favorited());
    }

    #[test]
    fn test_idle_age_returns_none_for_non_idle() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Running;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::seconds(60));
        // A Running session never has an idle age, even if a stale
        // `idle_entered_at` timestamp is sitting around (e.g. a transition
        // that bumped from Idle → Running but missed the cleanup path).
        assert_eq!(inst.idle_age(), None);
    }

    #[test]
    fn test_idle_age_returns_none_when_no_timestamp() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = None;
        assert_eq!(inst.idle_age(), None);
    }

    #[test]
    fn test_idle_age_returns_positive_duration() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::seconds(5));
        let age = inst.idle_age().expect("idle age should be present");
        // Allow generous slack so the test isn't flaky on slow CI.
        assert!(age.as_secs() >= 4 && age.as_secs() <= 30);
    }

    #[test]
    fn test_idle_age_clamps_negative_to_none() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        // Future timestamp (clock skew, hand-crafted state). `to_std()` on a
        // negative `chrono::Duration` returns Err, which we map to None so
        // the freshness logic sees "fully decayed" rather than panicking
        // or treating the session as freshly stopped.
        inst.idle_entered_at = Some(Utc::now() + chrono::Duration::seconds(60));
        assert_eq!(inst.idle_age(), None);
    }

    #[test]
    fn test_has_recent_activity_active_statuses_are_true() {
        let window = std::time::Duration::from_secs(15 * 60);
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Starting,
            Status::Creating,
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            assert!(
                inst.has_recent_activity(window),
                "{status:?} should keep the machine awake"
            );
        }
    }

    #[test]
    fn test_has_recent_activity_inactive_statuses_are_false() {
        let window = std::time::Duration::from_secs(15 * 60);
        for status in [
            Status::Stopped,
            Status::Error,
            Status::Unknown,
            Status::Deleting,
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            assert!(
                !inst.has_recent_activity(window),
                "{status:?} must not hold the sleep-inhibit assertion"
            );
        }
    }

    #[test]
    fn test_has_recent_activity_idle_within_window_is_true() {
        let window = std::time::Duration::from_secs(15 * 60);
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::seconds(60));
        assert!(inst.has_recent_activity(window));
    }

    #[test]
    fn test_has_recent_activity_idle_past_window_is_false() {
        let window = std::time::Duration::from_secs(15 * 60);
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::minutes(30));
        assert!(!inst.has_recent_activity(window));
    }

    #[test]
    fn test_has_recent_activity_idle_without_timestamp_is_false() {
        let window = std::time::Duration::from_secs(15 * 60);
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = None;
        assert!(!inst.has_recent_activity(window));
    }
    #[test]
    fn archive_settles_live_interaction_status_to_idle() {
        for status in [Status::Running, Status::Waiting, Status::Starting] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            inst.archive();
            assert!(inst.is_archived());
            assert_eq!(
                inst.status,
                Status::Idle,
                "{status:?} cannot be true of a row whose tmux archive tore down"
            );
        }
    }

    #[test]
    fn archive_leaves_resting_statuses_alone() {
        for status in [
            Status::Idle,
            Status::Stopped,
            Status::Error,
            Status::Unknown,
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            inst.archive();
            assert_eq!(inst.status, status, "{status:?} should survive archive");
        }
    }
}
