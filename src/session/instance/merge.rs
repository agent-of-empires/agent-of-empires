//! Reconciling one in-memory `Instance` against another: peer writes,
//! runtime reloads, TUI edits, and tool swaps.

use super::*;

impl Instance {
    /// Mutates launch-owned state. A strictly newer lifecycle generation also
    /// imports its status timestamps, capture floor, and error snapshot as one unit.
    pub fn merge_post_start(&mut self, src: &Self) {
        if src.lifecycle_generation < self.lifecycle_generation {
            return;
        }
        if src.lifecycle_generation > self.lifecycle_generation {
            self.idle_entered_at = src.idle_entered_at;
            self.last_accessed_at = src.last_accessed_at;
            self.last_error = src.last_error.clone();
            self.last_error_check = src.last_error_check;
        }
        self.lifecycle_generation = src.lifecycle_generation;
        self.status = src.status;
        self.sandbox_info = src.sandbox_info.clone();
        self.capture_started_at = src.capture_started_at;
    }

    /// Same fields as `merge_post_start`. Resume-probe failure markers are
    /// copied only when the sid still matches so peer poller writes that land
    /// between phase 2 and phase 3 of the restart remain authoritative.
    pub fn merge_post_restart(&mut self, src: &Self) {
        if src.lifecycle_generation < self.lifecycle_generation {
            return;
        }
        self.merge_post_start(src);
        if self.agent_session_id == src.agent_session_id {
            self.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
        }
    }

    pub fn merge_post_restart_with_baseline(&mut self, before: &Self, src: &Self) {
        if src.lifecycle_generation < self.lifecycle_generation {
            return;
        }
        self.merge_post_start(src);
        let generation_can_merge = self.omp_capture_generation == before.omp_capture_generation
            || self.omp_capture_generation == src.omp_capture_generation;
        self.lifecycle_generation = src.lifecycle_generation;
        let sid_unchanged = self.agent_session_id == before.agent_session_id;
        let marker_unchanged = self.resume_probe_failed_sid == before.resume_probe_failed_sid;

        if generation_can_merge {
            self.omp_capture_generation = src.omp_capture_generation.clone();
            self.session_id_poller = src.session_id_poller.clone();
            self.session_id_poller_retry_after = src.session_id_poller_retry_after;
            if sid_unchanged {
                self.agent_session_id = src.agent_session_id.clone();
            }
        } else if src.session_id_poller_is_running() {
            // A concurrent launch already published a third generation. The
            // restarted poller reloads tmux metadata on every tick, so keep
            // that live worker and let it rebind to the newer generation
            // without overwriting the newer durable identity.
            self.session_id_poller = src.session_id_poller.clone();
        }
        if generation_can_merge && marker_unchanged && self.agent_session_id == src.agent_session_id
        {
            self.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
        }
    }

    /// Carry runtime-only state across a storage reload without constructing a
    /// lifecycle snapshot from two different generations.
    ///
    /// `status` and `idle_entered_at` ARE generation-governed: a strictly newer
    /// disk snapshot (a peer's `commit_reserved_lifecycle_status`) must win over
    /// the stale in-memory copy. A Purge reservation is the exception: its
    /// generation bump deliberately leaves the durable status unchanged, so an
    /// in-memory `Deleting` overlay stays authoritative until the result
    /// arrives. `last_error`/`last_error_check`,
    /// `ever_confirmed_present`, and
    /// `unknown_since` are NOT generation-governed: no lifecycle writer
    /// (`reserve_/commit_/advance_lifecycle_generation`) produces an
    /// authoritative peer value for them. The reachability sentinels are
    /// serde-skipped, and the only on-disk error value is the one
    /// `reconcile_from_disk` round-trips back from this same in-memory poller
    /// state. The in-memory values therefore always win. Gating them on the
    /// generation would let an unrelated bump discard a poller's confirmed
    /// reachability and unknown streak, or a freshly derived
    /// `TMUX_SESSION_GONE_ERROR`, leaving the row stuck at `Error`+`None`.
    pub(crate) fn merge_runtime_from_reload(&mut self, previous: &Self) {
        let purge_in_flight = previous.status == Status::Deleting
            && self.lifecycle_reservation_is_owned(
                LifecycleOperation::Purge,
                self.lifecycle_generation,
            );
        if self.lifecycle_generation <= previous.lifecycle_generation || purge_in_flight {
            self.status = previous.status;
            self.idle_entered_at = previous.idle_entered_at;
        }
        // Reachability sentinels and detection bookkeeping are runtime-only
        // just like poller errors. A lifecycle generation bump does not make
        // serde-skipped defaults from disk authoritative, and the TUI's
        // heartbeat reload lands between two poll cycles: dropping `detection`
        // here loses the proposal awaiting its confirming poll (#3642).
        self.ever_confirmed_present = previous.ever_confirmed_present;
        self.unknown_since = previous.unknown_since;
        self.detection = previous.detection;
        self.last_error = previous.last_error.clone();
        self.last_error_check = previous.last_error_check;
        self.last_start_time = previous.last_start_time;
        self.session_id_poller = previous.session_id_poller.clone();
        self.session_id_poller_retry_after = previous.session_id_poller_retry_after;
        self.retroactive_capture_excludes = previous.retroactive_capture_excludes.clone();
    }

    /// Carry every in-process field from a pre-move live row onto the
    /// committed disk-derived candidate published by `HomeView`.
    /// Adding a new `#[serde(skip)]` field requires deciding whether
    /// `merge_runtime_from_reload`, this function, and
    /// `server::merge_runtime_fields` must carry it.
    pub(crate) fn merge_runtime_for_profile_move(&mut self, previous: &Self) {
        self.merge_runtime_from_reload(previous);
        self.live_status_baseline = previous.live_status_baseline;
        self.ever_confirmed_present = previous.ever_confirmed_present;
        self.unknown_since = previous.unknown_since;
        self.pane_dead_observed = previous.pane_dead_observed;
        self.force_fresh_next_launch = previous.force_fresh_next_launch;
        self.pending_host_env = previous.pending_host_env.clone();
        self.file_watch = previous.file_watch.clone();
        if let (Some(reloaded_sandbox), Some(runtime_sandbox)) =
            (self.sandbox_info.as_mut(), previous.sandbox_info.as_ref())
        {
            reloaded_sandbox.before_start_env = runtime_sandbox.before_start_env.clone();
        }
    }

    /// Splice TUI-mirrored, persisted fields from `src` onto `self`. Used by
    /// `HomeView::save` for fields the TUI is the canonical disk writer of
    /// (the daemon's `status_poll_loop` keeps these in memory only). The
    /// server's `send_message` respawn briefly writes `status` via
    /// `apply_post_restart_sync`; the resulting transient mis-paint
    /// converges on the next `status_poll` tick.
    /// User-action fields (archived/favorited/snoozed/title/group_path/...)
    /// are NOT here; they go through `apply_user_action` per-action so peer
    /// writers (CLI) cannot be clobbered by a stale TUI snapshot.
    pub fn merge_from_tui(&mut self, src: &Self) {
        if src.lifecycle_generation >= self.lifecycle_generation {
            self.lifecycle_generation = src.lifecycle_generation;
            self.status = src.status;
            self.last_accessed_at = self.last_accessed_at.max(src.last_accessed_at);
            self.idle_entered_at = src.idle_entered_at;
        }
        // Launch-config fields are TUI-authoritative and only mutated after
        // creation by the restart dialog (engine / command / args swap). They
        // have no peer writer, so a plain copy is safe. Syncing them here is
        // required: `reconcile_from_disk`'s `*self = disk` reload runs on every
        // launch, so a swap that never reached disk is silently reverted and
        // the session respawns with its original tool. See #switching-tools.
        self.tool = src.tool.clone();
        self.command = src.command.clone();
        self.extra_args = src.extra_args.clone();
    }

    /// Move this row to a different `tool` (the TUI restart dialog's engine
    /// swap), parking the outgoing agent's session ids and picking up the
    /// incoming agent's, if it has been here before.
    ///
    /// Session ids live in per-agent namespaces: a Claude UUID means nothing
    /// to codex or gemini, but `is_valid_session_id` accepts any shape, so a
    /// carried-over sid makes the next launch emit `--resume <foreign-sid>`
    /// and the new engine starts by failing to resume. #3077 made the swap
    /// reach disk, which is what exposed this. The rest of what this clears
    /// mirrors the structured-view agent switch (`POST /api/acp/:id/switch`).
    ///
    /// A no-op when `new_tool` is the current tool, so a caller may apply it
    /// to a disk row and an in-memory row independently without the second
    /// call double-stashing.
    ///
    /// Callers must persist the result themselves: `merge_from_tui`
    /// deliberately does not sync these fields (the capture pollers own
    /// `agent_session_id` through CAS writes), so an in-memory-only swap is
    /// reverted by `reconcile_from_disk` on the next launch.
    pub(crate) fn swap_tool(&mut self, new_tool: &str) {
        if new_tool == self.tool {
            return;
        }
        // Park the outgoing agent's conversation under its own name so a swap
        // back to it resumes there instead of starting a third conversation.
        let outgoing = PriorToolSession {
            agent_session_id: self.agent_session_id.take(),
            acp_session_id: self.acp_session_id.take(),
        };
        if !outgoing.is_empty() {
            self.prior_tool_session_ids
                .insert(self.tool.clone(), outgoing);
        }
        self.tool = new_tool.to_string();
        // The alias is resolved per-tool, so the outgoing tool's answer cannot
        // survive: kept, it points `resolved_agent` at the wrong built-in
        // outright (a `codex-personal` -> `claude-personal` swap would keep
        // detecting as codex); cleared, the row lands in the same
        // empty-`detect_as` state a session built before its tool joined
        // `[session.agent_detect_as]` does. Re-resolve against the same
        // process-global registry `effective_detect_as` reads, so this stays a
        // lookup rather than a config load, and the row ends up exactly as if
        // it had been built on the new tool.
        self.detect_as =
            tmux::status_rules::effective_detect_as(&self.source_profile, new_tool, "")
                .into_owned();
        // Consumed, not copied: the row owns exactly one live conversation per
        // agent, and leaving the entry behind would let a later swap restore an
        // id this session has since replaced.
        let restored = self
            .prior_tool_session_ids
            .remove(new_tool)
            .unwrap_or_default();
        self.agent_session_id = restored.agent_session_id;
        self.acp_session_id = restored.acp_session_id;
        self.resume_probe_failed_sid = None;
        // A pin/clear/fork directive names an id in the old agent's namespace,
        // so it cannot survive the swap either.
        self.resume_intent = ResumeIntent::Default;
        // Effort vocabularies are adapter-specific, so the old agent's pick is
        // meaningless to the new one; it falls back to the new agent's default.
        self.acp_effort = None;
        // Same for the pinned model: `claude-opus-4-7` means nothing to codex,
        // and it is re-injected on every spawn, so it has to go too.
        self.agent_model = None;
        // `acp_mode_id` deliberately stays. It is the session's approval
        // posture, and clearing it does not fall back to "default": the spawn
        // path's mode gate is `acp_mode_id.is_some() || yolo_mode`, whose
        // `None` arm resolves the adapter's *bypass* mode id, so dropping an
        // explicit restrictive mode from a `yolo_mode` row would silently
        // escalate the new agent to auto-approve. An unrecognized mode id is a
        // warn-and-continue no-op instead, which is the safe failure. The
        // structured-view agent switch passes it through for the same reason.
        self.import_pending = None;
        self.fork_pending = None;
        // The pinned structured-view agent belongs to the old tool; clearing it
        // lets the spawn path pick the new tool's default agent instead of
        // silently keeping the old backend alive across the swap.
        self.agent_name = None;
    }

    /// Apply a passively-detected status transition to a disk row. Touches
    /// the same three fields as [`Self::merge_from_tui`] (`status`,
    /// `idle_entered_at`, `last_accessed_at`); the real distinction is the
    /// API shape (a minimal [`PassiveStatusPatch`] rather than a full
    /// `Self`) and the merge policy on `last_accessed_at`: `merge_from_tui`
    /// takes the monotone max, this drops the incoming `last_accessed_at`
    /// outright when disk already has a strictly newer one, so a
    /// poller-produced patch loses to a newer explicit user touch instead of
    /// racing it.
    ///
    /// `status`/`idle_entered_at` apply independently of timestamp only while
    /// the patch's lifecycle generation is current. This prevents an old pane
    /// poll from repainting a newer Stop/Restart/Archive commit.
    ///
    /// The `>=` guard on `last_accessed_at` compares `chrono::Utc::now()`
    /// values, which delegate to `SystemTime::now()` (wall clock, not
    /// monotonic). Under an NTP rewind, a genuinely newer live observation
    /// stamped after the rewind can compare less than a value stamped
    /// before it and be silently dropped. Best-effort monotone, not a hard
    /// guarantee; the next poll tick converges regardless.
    ///
    /// A `last_accessed_at` older-or-equal to disk is silently dropped
    /// (the `>=` guard) with a `session.store` debug log at drop time,
    /// while `status` and `idle_entered_at` still apply unconditionally.
    /// Callers relying on the observable `last_accessed_at` change must
    /// re-read the field after `merge_passive_status_patch` returns.
    pub(crate) fn merge_passive_status_patch(&mut self, id: &str, patch: &PassiveStatusPatch) {
        if patch.lifecycle_generation < self.lifecycle_generation {
            tracing::debug!(
                target: "session.store",
                session_id = %id,
                patch_generation = patch.lifecycle_generation,
                disk_generation = self.lifecycle_generation,
                "dropped passive status patch from an older lifecycle generation"
            );
            return;
        }
        self.lifecycle_generation = patch.lifecycle_generation;
        self.status = patch.status;
        self.idle_entered_at = patch.idle_entered_at;
        let Some(incoming) = patch.last_accessed_at else {
            return;
        };
        if self.last_accessed_at.is_some_and(|disk| disk >= incoming) {
            tracing::debug!(
                target: "session.store",
                session_id = %id,
                disk_ts = ?self.last_accessed_at,
                patch_ts = %incoming,
                "dropped passive status patch's last_accessed_at as a no-op (disk value is at least as recent; status/idle_entered_at still applied)"
            );
            return;
        }
        self.last_accessed_at = Some(incoming);
    }

    /// Merge the complete user-requested delta for a cross-profile move while
    /// preserving unrelated fields refreshed by a peer after `pre` was read.
    /// A tool change is one atomic state transition: the tool name and every
    /// conversation field staged by `swap_tool` must travel together.
    pub(crate) fn merge_profile_move_diff(&mut self, pre: &Self, post: &Self) {
        self.merge_user_action_diff(pre, post);
        if pre.tool != post.tool {
            // Apply the requested transition to the freshly locked disk row.
            // The TUI post snapshot can carry parked session ids captured
            // before a poller or peer refreshed the durable conversation state.
            self.swap_tool(&post.tool);
        }
        if pre.command != post.command {
            self.command = post.command.clone();
        }
        if pre.extra_args != post.extra_args {
            self.extra_args = post.extra_args.clone();
        }
    }

    /// Per-field-conditional splice: copy `post.X` onto `self.X` only when
    /// `pre.X != post.X`. Peer writes to fields the mutation did not touch
    /// survive even when the field is in the user-action set.
    /// `last_accessed_at` is monotone-max (no diff guard).
    /// `source_profile` is excluded from this splice. Same-profile actions call
    /// this directly; cross-profile moves call it through
    /// `merge_profile_move_diff` and assign `source_profile` separately.
    /// Post-splice rules enforce the same cross-field invariants the
    /// per-mutation methods enforce (archive XOR favorite, touch unarchives)
    /// so concurrent peer writes cannot violate them.
    pub fn merge_user_action_diff(&mut self, pre: &Self, post: &Self) {
        debug_assert_eq!(
            pre.source_profile, post.source_profile,
            "apply_user_action must not change source_profile; cross-profile moves go through mutate_instance"
        );
        if pre.title != post.title {
            self.title = post.title.clone();
        }
        if pre.group_path != post.group_path {
            self.group_path = post.group_path.clone();
        }
        if pre.archived_at != post.archived_at {
            self.archived_at = post.archived_at;
        }
        if pre.favorited_at != post.favorited_at {
            self.favorited_at = post.favorited_at;
        }
        if pre.snoozed_until != post.snoozed_until {
            self.snoozed_until = post.snoozed_until;
        }
        if pre.pinned_at != post.pinned_at {
            self.pinned_at = post.pinned_at;
        }
        if pre.trashed_at != post.trashed_at {
            self.trashed_at = post.trashed_at;
        }
        if pre.pre_trash_project_path != post.pre_trash_project_path {
            self.pre_trash_project_path = post.pre_trash_project_path.clone();
        }
        if pre.unread != post.unread {
            self.unread = post.unread;
        }
        if pre.base_branch_override != post.base_branch_override {
            self.base_branch_override = post.base_branch_override.clone();
        }
        if pre.color != post.color {
            self.color = post.color.clone();
        }
        // Worktree workdir edit (move dir / rename branch) mutates these two;
        // both the TUI and the CLI can write them, so they go through the
        // same conditional-diff path as the triage fields. See #1723.
        if pre.project_path != post.project_path {
            self.project_path = post.project_path.clone();
        }
        if pre.worktree_info != post.worktree_info {
            self.worktree_info = post.worktree_info.clone();
        }
        // `workspace_info` deliberately has NO arm. Attaching a project (#3103)
        // converts the session into a workspace, but it does that through
        // `Storage::update` (which takes both lock layers) rather than through a
        // user-action diff, so the value on disk is already authoritative here.
        // Assigning `post`'s copy would let a stale TUI snapshot clobber a
        // conversion a peer landed between the `pre` snapshot and this merge.
        // `status` deliberately has no arm. It is runtime state, not user
        // intent; copying it from a stale TUI snapshot could overwrite a
        // lifecycle transition loaded under the storage lock.
        // Lifecycle ownership is intentionally never spliced from a TUI
        // snapshot. Only transition code holding the per-instance flock may
        // mutate the durable reservation and generation.
        self.last_accessed_at = self.last_accessed_at.max(post.last_accessed_at);

        let archived_changed = pre.archived_at != post.archived_at;
        let favorited_changed = pre.favorited_at != post.favorited_at;
        let snoozed_changed = pre.snoozed_until != post.snoozed_until;
        let pinned_changed = pre.pinned_at != post.pinned_at;
        // Touch is an event invariant: any advance of last_accessed_at
        // (TUI-side or peer-side) dethrones a concurrent archive.
        let touched = self.last_accessed_at > pre.last_accessed_at;

        // archive(): archived=Some => favorited=None, snoozed=None, pinned=None
        if archived_changed && post.archived_at.is_some() {
            self.favorited_at = None;
            self.snoozed_until = None;
            self.pinned_at = None;
        }
        // favorite(): favorited=Some => archived=None, snoozed=None
        if favorited_changed && post.favorited_at.is_some() {
            self.archived_at = None;
            self.snoozed_until = None;
        }
        // snooze(): snoozed=Some => pinned=None (sink clears surface).
        if snoozed_changed && post.snoozed_until.is_some() {
            self.pinned_at = None;
        }
        // pin(): pinned=Some => archived=None, snoozed=None (surface clears sinks).
        if pinned_changed && post.pinned_at.is_some() {
            self.archived_at = None;
            self.snoozed_until = None;
        }
        // touch_last_accessed(): clears archived + snoozed + idle-dormant.
        // Does NOT clear favorite or pin (both are explicit user-surfacing
        // signals, not sink states). Mirrors touch_last_accessed() so the
        // wake-from-dormancy invariant holds on the concurrent-writer merge
        // path too, not just direct touches (#1689).
        if touched {
            self.archived_at = None;
            self.snoozed_until = None;
            self.idle_dormant_since = None;
        }
        // Final-state invariant: archive is the strongest dismiss and
        // wins over snooze. The per-mutation rules above clear other
        // flags on the change side, but the diff can also leave disk
        // archived (pre-existing) AND snoozed (added by post); without
        // this check the row would persist both and the web sidebar's
        // tier comparator (which assumes exactly one active triage
        // state) would render contradictory chips. See #1581.
        if self.archived_at.is_some() {
            self.snoozed_until = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    use tracing_test::traced_test;

    #[test]
    fn test_merge_user_action_diff_propagates_unread() {
        let pre = Instance::new("t", "/tmp");
        let mut post = pre.clone();
        post.unread = true;
        let mut disk = pre.clone();
        disk.merge_user_action_diff(&pre, &post);
        assert!(disk.unread);

        // Clearing also propagates.
        let pre2 = post.clone();
        let mut post2 = pre2.clone();
        post2.unread = false;
        let mut disk2 = pre2.clone();
        disk2.merge_user_action_diff(&pre2, &post2);
        assert!(!disk2.unread);
    }

    #[test]
    fn test_merge_user_action_diff_propagates_trash_marker() {
        let pre = Instance::new("t", "/tmp");
        let mut post = pre.clone();
        post.trash();
        let mut disk = pre.clone();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.is_trashed());

        let pre2 = post.clone();
        let mut post2 = pre2.clone();
        post2.untrash();
        let mut disk2 = pre2.clone();

        disk2.merge_user_action_diff(&pre2, &post2);
        assert!(!disk2.is_trashed());
    }

    #[test]
    fn test_merge_post_start_imports_newer_lifecycle_snapshot_as_a_unit() {
        let stale_idle = Utc::now() - chrono::Duration::minutes(5);
        let mut live = Instance::new("session", "/tmp/test");
        live.lifecycle_generation = 7;
        live.status = Status::Starting;
        live.idle_entered_at = Some(stale_idle);
        live.last_error = Some("stale pane observation".to_string());
        let stale_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        live.capture_started_at = Some(stale_floor);

        let mut disk = live.clone();
        disk.lifecycle_generation = 8;
        disk.status = Status::Stopped;
        disk.idle_entered_at = None;
        disk.last_error = None;
        let launched_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        disk.capture_started_at = Some(launched_floor);

        live.merge_post_start(&disk);

        assert_eq!(live.lifecycle_generation, 8);
        assert_eq!(live.status, Status::Stopped);
        assert_eq!(live.idle_entered_at, None);
        assert_eq!(live.last_error, None);
        assert_eq!(live.capture_started_at, Some(launched_floor));
    }

    #[test]
    fn runtime_reload_keeps_strictly_newer_disk_lifecycle_snapshot() {
        let mut previous = Instance::new("session", "/tmp/test");
        previous.lifecycle_generation = 3;
        previous.status = Status::Starting;
        previous.idle_entered_at = Some(Utc::now());
        previous.last_error = Some("old observation".to_string());
        let previous_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        previous.capture_started_at = Some(previous_floor);

        previous.detection = DetectionState {
            pending: Some(Status::Idle),
            ..Default::default()
        };

        let mut reloaded = previous.clone();
        reloaded.lifecycle_generation = 4;
        reloaded.status = Status::Stopped;
        reloaded.idle_entered_at = None;
        reloaded.last_error = None;
        let committed_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        reloaded.capture_started_at = Some(committed_floor);
        // A disk load leaves every `#[serde(skip)]` field at its default.
        reloaded.detection = DetectionState::default();
        reloaded.merge_runtime_from_reload(&previous);

        // Generation-governed fields: the strictly-newer disk snapshot wins.
        assert_eq!(reloaded.lifecycle_generation, 4);
        assert_eq!(reloaded.status, Status::Stopped);
        assert_eq!(reloaded.idle_entered_at, None);
        // last_error is runtime-only: the in-memory poller value survives even a
        // newer generation, since no lifecycle writer persists last_error.
        assert_eq!(reloaded.last_error.as_deref(), Some("old observation"));
        assert_eq!(
            reloaded.capture_started_at,
            Some(committed_floor),
            "reload must retain the exact launch-owned floor from disk"
        );
        // So is the detection bookkeeping: a reload between two poll cycles
        // must not drop a proposal awaiting its confirming poll (#3642).
        assert_eq!(reloaded.detection.pending, Some(Status::Idle));

        let mut same_generation_disk = previous.clone();
        same_generation_disk.capture_started_at = Some(committed_floor);
        same_generation_disk.merge_runtime_from_reload(&previous);
        assert_eq!(
            same_generation_disk.capture_started_at,
            Some(committed_floor),
            "a stale same-generation runtime snapshot must not replace a committed disk floor"
        );

        let mut deleting = Instance::new("deleting", "/tmp/test");
        deleting.lifecycle_generation = 3;
        deleting.status = Status::Deleting;

        let mut reserved = deleting.clone();
        reserved.lifecycle_generation = 4;
        reserved.status = Status::Idle;
        reserved.lifecycle_reservation = Some(LifecycleReservation {
            op: LifecycleOperation::Purge,
            generation: 4,
            at: Utc::now(),
        });
        reserved.merge_runtime_from_reload(&deleting);

        assert_eq!(reserved.lifecycle_generation, 4);
        assert_eq!(reserved.status, Status::Deleting);

        let mut launch_reserved = reserved.clone();
        launch_reserved.status = Status::Stopped;
        launch_reserved.lifecycle_reservation.as_mut().unwrap().op = LifecycleOperation::Launch;
        launch_reserved.merge_runtime_from_reload(&deleting);

        assert_eq!(launch_reserved.lifecycle_generation, 4);
        assert_eq!(
            launch_reserved.status,
            Status::Stopped,
            "only a Purge reservation may preserve the Deleting overlay"
        );
    }

    #[test]
    fn runtime_reload_preserves_reachability_sentinels_across_generation_bump() {
        let mut previous = Instance::new("session", "/tmp/test");
        previous.lifecycle_generation = 3;
        previous.ever_confirmed_present = true;
        let unknown_since = std::time::Instant::now() - std::time::Duration::from_secs(2);
        previous.unknown_since = Some(unknown_since);

        let mut reloaded = Instance::new("session", "/tmp/test");
        reloaded.lifecycle_generation = 4;
        reloaded.merge_runtime_from_reload(&previous);

        assert!(reloaded.ever_confirmed_present);
        assert_eq!(reloaded.unknown_since, Some(unknown_since));
    }

    #[test]
    fn runtime_reload_preserves_poller_gone_error_across_generation_bump() {
        // A stop/unarchive bumps the disk generation with status: None, so the
        // reloaded row carries no last_error. The poller's freshly derived
        // TMUX_SESSION_GONE_ERROR (in memory) must survive, or the row freezes
        // at Error+None and the stopped preview never renders (#3230).
        let mut previous = Instance::new("session", "/tmp/test");
        previous.lifecycle_generation = 7;
        previous.status = Status::Error;
        previous.last_error = Some(TMUX_SESSION_GONE_ERROR.to_string());

        let mut reloaded = previous.clone();
        reloaded.lifecycle_generation = 8;
        reloaded.status = Status::Error;
        reloaded.last_error = None;
        reloaded.merge_runtime_from_reload(&previous);

        assert_eq!(
            reloaded.last_error.as_deref(),
            Some(TMUX_SESSION_GONE_ERROR)
        );
    }

    #[test]
    fn test_merge_post_start_preserves_peer_field_writes() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.archive();
        stored.agent_session_id = Some("daemon-sid".to_string());

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Starting;

        stored.merge_post_start(&working);

        assert_eq!(stored.status, Status::Starting);
        assert!(stored.is_archived(), "peer archive must survive merge");
        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("daemon-sid"),
            "peer-written sid must survive merge"
        );

        stored.lifecycle_generation = 2;
        stored.status = Status::Stopped;
        let winning_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        stored.capture_started_at = Some(winning_floor);
        working.lifecycle_generation = 1;
        working.status = Status::Starting;
        working.capture_started_at =
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000));
        stored.merge_post_start(&working);
        assert_eq!(stored.status, Status::Stopped);
        assert_eq!(stored.capture_started_at, Some(winning_floor));
        stored.merge_from_tui(&working);
        assert_eq!(
            stored.status,
            Status::Stopped,
            "a stale async/TUI result must not overwrite a newer lifecycle commit"
        );
    }

    #[test]
    fn test_merge_post_restart_preserves_peer_sid() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("peer-fresh-sid".to_string());
        stored.snooze(15);

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Idle;
        working.agent_session_id = Some("phase1-stale-sid".to_string());

        stored.merge_post_restart(&working);

        assert_eq!(stored.status, Status::Idle);
        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("peer-fresh-sid"),
            "restart merge must not clobber peer sid write"
        );
        assert!(stored.is_snoozed(), "peer snooze must survive merge");

        let mut before = Instance::new("omp-session", "/tmp/test");
        before.agent_session_id = Some("old-sid".to_string());
        before.omp_capture_generation = Some("generation-a".to_string());
        let mut restarted = before.clone();
        restarted.omp_capture_generation = Some("generation-b".to_string());
        let mut poller = crate::session::poller::SessionPoller::new("omp-restarted".to_string());
        assert!(poller.start(before.id.clone(), Box::new(|| None), Box::new(|_| {}), None,));
        let restarted_poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
        restarted.session_id_poller = Some(restarted_poller.clone());
        let mut live = before.clone();
        live.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(live.omp_capture_generation.as_deref(), Some("generation-b"));
        assert!(live.session_id_poller.is_some());

        let mut generation_converged = before.clone();
        generation_converged.agent_session_id = Some("peer-sid".to_string());
        generation_converged.omp_capture_generation = Some("generation-b".to_string());
        generation_converged.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(
            generation_converged.agent_session_id.as_deref(),
            Some("peer-sid")
        );
        assert!(generation_converged.session_id_poller.is_some());

        let mut peer_relaunched = before.clone();
        peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
        peer_relaunched.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(
            peer_relaunched.omp_capture_generation.as_deref(),
            Some("peer-generation")
        );
        assert!(std::sync::Arc::ptr_eq(
            peer_relaunched
                .session_id_poller
                .as_ref()
                .expect("running restart poller"),
            &restarted_poller,
        ));
        restarted_poller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stop();
    }

    #[test]
    fn test_merge_post_restart_copies_resume_failed_marker_when_sid_matches() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("failed-sid".to_string());
        stored.resume_probe_failed_sid = None;

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Error;
        working.agent_session_id = Some("failed-sid".to_string());
        working.resume_probe_failed_sid = Some("failed-sid".to_string());

        stored.merge_post_restart(&working);

        assert_eq!(stored.status, Status::Error);
        assert_eq!(stored.agent_session_id.as_deref(), Some("failed-sid"));
        assert_eq!(
            stored.resume_probe_failed_sid.as_deref(),
            Some("failed-sid")
        );
    }

    #[test]
    fn test_merge_post_restart_preserves_peer_marker_when_sid_mismatches() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.agent_session_id = Some("poller-fresh-sid".to_string());
        stored.resume_probe_failed_sid = Some("poller-fresh-sid".to_string());

        let mut working = Instance::new("session", "/tmp/test");
        working.id = stored.id.clone();
        working.status = Status::Starting;
        working.agent_session_id = Some("phase1-stale-sid".to_string());
        working.resume_probe_failed_sid = Some("phase1-stale-sid".to_string());

        stored.merge_post_restart(&working);

        assert_eq!(
            stored.agent_session_id.as_deref(),
            Some("poller-fresh-sid"),
            "poller wrote a fresh sid between phase 2 and phase 3; merge preserves it"
        );
        assert_eq!(
            stored.resume_probe_failed_sid.as_deref(),
            Some("poller-fresh-sid"),
            "marker for peer sid remains authoritative"
        );
    }

    #[test]
    fn test_merge_diff_peer_archive_loses_to_tui_favorite() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.favorite();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.favorited_at.is_some(), "TUI favorite landed");
        assert!(
            disk.archived_at.is_none(),
            "favorite() invariant must clear concurrent peer archive"
        );
    }

    #[test]
    fn test_merge_diff_peer_favorite_loses_to_tui_archive() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.archive();

        let mut disk = pre.clone();
        disk.favorite();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.archived_at.is_some(), "TUI archive landed");
        assert!(
            disk.favorited_at.is_none(),
            "archive() invariant must clear concurrent peer favorite"
        );
    }

    #[test]
    fn test_merge_diff_peer_archive_loses_to_tui_touch() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.touch_last_accessed();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(
            disk.archived_at.is_none(),
            "touch_last_accessed() invariant must clear concurrent peer archive"
        );
    }

    #[test]
    fn test_merge_diff_peer_touch_clears_tui_archive() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));

        let mut post = pre.clone();
        post.archive();

        let mut disk = pre.clone();
        disk.touch_last_accessed();

        disk.merge_user_action_diff(&pre, &post);

        assert!(
            disk.archived_at.is_none(),
            "peer touch (newer last_accessed_at) must dethrone TUI archive per messaging-unarchives rule"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_merge_diff_passive_transition_stamp_does_not_wipe_concurrent_sink_state() {
        // #3465: a passive status transition restamped last_accessed_at
        // (update_status_with_metadata wrote Some(now) on every detected
        // transition, with no user gesture behind it), and the stamp
        // reached disk through PassiveStatusPatch while a user action was
        // in flight. The writer's stale pre snapshot then made the
        // deliberate touched arm read the advance as a peer touch and wipe
        // sink state the user had just set. That arm is correct for real
        // gestures (pinned by test_merge_diff_peer_touch_clears_tui_archive,
        // the messaging-unarchives rule); the poller stamp was the lie.
        //
        // Driven through the real transition path: the
        // update_status_with_metadata call below detects a genuine
        // Idle -> Error flip (session forced Absent, see #2936), which on
        // the pre-fix tree restamped last_accessed_at between the pre
        // snapshot and the merge.
        type SinkCase = (&'static str, fn(&mut Instance), fn(&Instance) -> bool);
        let cases: &[SinkCase] = &[
            // The issue's headline victim: a concurrent archive.
            ("archived_at", |i| i.archive(), |i| i.archived_at.is_some()),
            // Same touched arm, same wipe, for a concurrent snooze.
            (
                "snoozed_until",
                |i| i.snooze(15),
                |i| i.snoozed_until.is_some(),
            ),
        ];
        let user_touch = Utc::now() - chrono::Duration::seconds(60);
        for (field, seed_sink, sink_present) in cases {
            // Snapshot the acting writer held before the poller tick.
            let mut pre = Instance::new("s", "/tmp/x");
            pre.live_status_baseline = Some(Status::Idle);
            pre.status = Status::Idle;
            pre.last_accessed_at = Some(user_touch);

            // One passive poller tick observes Idle -> Error. On the
            // pre-fix tree this restamped last_accessed_at on the row that
            // lands on disk; post-fix it leaves the user-gesture stamp
            // alone and only updates idle_entered_at bookkeeping.
            let mut disk = pre.clone();
            let _cache = force_session_absent();
            disk.update_status_with_metadata(None, None);
            assert_eq!(disk.status, Status::Error);

            // The concurrent user action seeds the sink on the writer's
            // post snapshot.
            let mut post = pre.clone();
            seed_sink(&mut post);

            disk.merge_user_action_diff(&pre, &post);

            assert!(
                sink_present(&disk),
                "passive transition must not wipe concurrent {field} (#3465)"
            );
        }
    }

    #[test]
    fn test_merge_diff_peer_archive_clears_concurrent_tui_snooze() {
        // The web/TUI/CLI contract treats pinned/archived/snoozed as
        // mutually exclusive (the sidebar tier comparator assumes a
        // single active triage state, see #1581). When a TUI snooze
        // races a peer archive, archive wins: snooze is a temporary
        // sink and archive is the indefinite one, so leaving both set
        // would surface contradictory triage state on the next render.
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.snooze(15);

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.archived_at.is_some(), "peer archive survives");
        assert!(
            disk.snoozed_until.is_none(),
            "archive() invariant must clear a concurrent TUI snooze"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_merge_diff_passive_transition_stamp_does_not_wake_dormant_row() {
        // Dormancy is the third field the touched arm wipes (#3465), with
        // one structural difference from the archive/snooze cases: it is
        // never spliced from post, so the wipe only hits a value already
        // on the row. Seed it on the base instance, drive one passive
        // poller tick through the real transition path (session forced
        // Absent, see #2936), and confirm an unrelated user action does
        // not wake the row just because the pre-fix tree restamped
        // last_accessed_at in between.
        let mut pre = Instance::new("s", "/tmp/x");
        pre.live_status_baseline = Some(Status::Idle);
        pre.status = Status::Idle;
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));
        pre.idle_dormant_since = Some(Utc::now() - chrono::Duration::hours(5));

        let mut disk = pre.clone();
        let _cache = force_session_absent();
        disk.update_status_with_metadata(None, None);
        assert_eq!(disk.status, Status::Error);

        let mut post = pre.clone();
        post.favorite();
        disk.merge_user_action_diff(&pre, &post);

        assert!(
            disk.idle_dormant_since.is_some(),
            "a passive transition must not wake a dormant row (#3465)"
        );
    }

    #[test]
    fn test_merge_diff_tui_unfavorite_does_not_resurrect_peer_archive() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.favorite();

        let mut post = pre.clone();
        post.unfavorite();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.favorited_at.is_none(), "TUI unfavorite landed");
        assert!(
            disk.archived_at.is_some(),
            "post.favorited_at == None; favorite-invariant rule must NOT fire"
        );
    }

    #[test]
    fn test_merge_diff_preserves_runtime_state_and_peer_touch() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));
        pre.archived_at = Some(Utc::now() - chrono::Duration::seconds(120));

        let mut post = pre.clone();
        post.title = "renamed".into();
        post.status = Status::Running;

        let mut disk = pre.clone();
        disk.touch_last_accessed();
        disk.status = Status::Waiting;

        disk.merge_user_action_diff(&pre, &post);

        assert_eq!(disk.title, "renamed");
        assert!(disk.archived_at.is_none());
        assert_eq!(
            disk.status,
            Status::Waiting,
            "runtime status must remain authoritative"
        );
    }

    #[test]
    fn test_merge_diff_peer_archive_loses_to_tui_pin() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.pin();

        let mut disk = pre.clone();
        disk.archive();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.pinned_at.is_some(), "TUI pin landed");
        assert!(
            disk.archived_at.is_none(),
            "pin() invariant must clear concurrent peer archive"
        );
    }

    #[test]
    fn test_merge_diff_peer_pin_loses_to_tui_archive() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.archive();

        let mut disk = pre.clone();
        disk.pin();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.archived_at.is_some(), "TUI archive landed");
        assert!(
            disk.pinned_at.is_none(),
            "archive() invariant must clear concurrent peer pin"
        );
    }

    #[test]
    fn test_merge_diff_peer_pin_loses_to_tui_snooze() {
        let pre = Instance::new("s", "/tmp/x");
        let mut post = pre.clone();
        post.snooze(30);

        let mut disk = pre.clone();
        disk.pin();

        disk.merge_user_action_diff(&pre, &post);

        assert!(disk.snoozed_until.is_some(), "TUI snooze landed");
        assert!(
            disk.pinned_at.is_none(),
            "snooze() invariant must clear concurrent peer pin"
        );
    }

    #[test]
    fn test_merge_diff_peer_touch_preserves_pin() {
        let mut pre = Instance::new("s", "/tmp/x");
        pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));

        let mut post = pre.clone();
        post.pin();

        let mut disk = pre.clone();
        disk.touch_last_accessed();

        disk.merge_user_action_diff(&pre, &post);

        // Touch dethrones archive/snooze but NOT pin: pin is an explicit
        // surfacing signal that the user's interaction does not contradict.
        assert!(
            disk.pinned_at.is_some(),
            "peer touch must NOT clear concurrent TUI pin"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_applies_status_and_timestamps() {
        let mut disk = Instance::new("session", "/tmp/test");
        disk.status = Status::Running;
        disk.idle_entered_at = None;
        disk.last_accessed_at = Some(Utc::now() - chrono::Duration::hours(1));
        disk.title = "peer-title".to_string();
        disk.group_path = "peer/group".to_string();
        disk.unread = true;
        disk.archived_at = Some(Utc::now());
        disk.favorited_at = None;
        disk.pinned_at = Some(Utc::now());
        let before = disk.clone();

        let now = Utc::now();
        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(now),
            last_accessed_at: Some(now),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(now));
        assert_eq!(disk.last_accessed_at, Some(now));
        // Narrow splice: nothing else moves.
        assert_eq!(disk.title, before.title);
        assert_eq!(disk.group_path, before.group_path);
        assert_eq!(disk.unread, before.unread);
        assert_eq!(disk.archived_at, before.archived_at);
        assert_eq!(disk.favorited_at, before.favorited_at);
        assert_eq!(disk.pinned_at, before.pinned_at);

        disk.lifecycle_generation = 2;
        disk.status = Status::Stopped;
        let mut stale_lifecycle = patch.clone();
        stale_lifecycle.lifecycle_generation = 1;
        stale_lifecycle.status = Status::Running;
        disk.merge_passive_status_patch(&disk.id.clone(), &stale_lifecycle);
        assert_eq!(
            disk.status,
            Status::Stopped,
            "a poll from an older pane generation must not repaint Stop"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_never_fabricates_last_accessed_at() {
        // The source Instance was never touched by a user (last_accessed_at
        // itself None); the patch must preserve that rather than fabricate
        // a stamp, or a session that transitions status before anyone
        // attaches gains a spurious "touched" signal.
        let mut disk = Instance::new("session", "/tmp/test");
        disk.status = Status::Starting;
        disk.last_accessed_at = None;

        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(Utc::now()),
            last_accessed_at: None,
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.status, Status::Idle, "status must still apply");
        assert_eq!(
            disk.last_accessed_at, None,
            "must not fabricate a last_accessed_at the source never had"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_status_and_idle_entered_at_apply_even_when_last_accessed_at_is_stale(
    ) {
        // A peer (CLI, TUI apply_user_action) touched last_accessed_at more
        // recently than the passive patch's snapshot: only last_accessed_at
        // is guarded. status/idle_entered_at still apply, or a real status
        // transition would silently strand on disk until the next one.
        let mut disk = Instance::new("session", "/tmp/test");
        let peer_touch = Utc::now();
        disk.status = Status::Running;
        disk.last_accessed_at = Some(peer_touch);
        disk.idle_entered_at = None;

        let stale_patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(peer_touch - chrono::Duration::minutes(5)),
            last_accessed_at: Some(peer_touch - chrono::Duration::minutes(5)),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &stale_patch);

        assert_eq!(
            disk.status,
            Status::Idle,
            "status must apply even when last_accessed_at is stale"
        );
        assert_eq!(
            disk.idle_entered_at,
            Some(peer_touch - chrono::Duration::minutes(5)),
            "idle_entered_at must apply even when last_accessed_at is stale"
        );
        assert_eq!(
            disk.last_accessed_at,
            Some(peer_touch),
            "only last_accessed_at itself is guarded against the stale patch"
        );
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_equal_is_a_noop() {
        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        disk.last_accessed_at = Some(ts);

        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        // Guard is `>=`: equal timestamps are not a real advance, so the
        // patch's last_accessed_at is dropped. The observable value stays
        // equal to `ts` either way (disk == incoming), so the assertion
        // does not change; the point of the guard is skipping the write.
        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    /// Count the guard's drop-event log lines. `logs_assert` hands us lines
    /// already scoped to the calling test's span, and the message is unique to
    /// the drop branch, so matching the substring cannot be inflated by other
    /// `session.store` events.
    fn drop_log_count(lines: &[&str]) -> usize {
        lines
            .iter()
            .filter(|l| l.contains("dropped passive status patch's last_accessed_at as a no-op"))
            .count()
    }

    /// Closes I4 from #2756: the equal-timestamp guard's observability gap.
    /// Under `disk == incoming` the drop branch and the write branch leave the
    /// same observable `last_accessed_at`, so `boundary_equal_is_a_noop` above
    /// cannot prove the drop branch ran. Here `disk == incoming` must fire the
    /// `session.store` drop log exactly once.
    #[traced_test]
    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_equal_logs_drop_event() {
        // Tracing caches per-callsite `Interest` globally on first hit, so a
        // parallel test that reaches the drop callsite first without a
        // capturing subscriber pins it to `Interest::never()` and this
        // capture silently sees zero lines. Re-evaluate the (already
        // registered) callsite against `traced_test`'s subscriber first. Same
        // race `run_with_capture` documents in session::deletion.
        tracing::callsite::rebuild_interest_cache();

        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        disk.last_accessed_at = Some(ts);

        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        logs_assert(|lines: &[&str]| match drop_log_count(lines) {
            1 => Ok(()),
            n => Err(format!("expected 1 drop event, got {n}")),
        });
    }

    /// Closes I4 from #2756 (write side): a strictly newer incoming timestamp
    /// skips the guard, so the drop log must fire zero times and the value is
    /// written. Pairing the zero-count write case with the exactly-once drop
    /// case above proves the log is a faithful drop-vs-write signal, not a line
    /// that fires regardless. Uses an explicit minute offset (as
    /// `boundary_newer_applies` does) to avoid a same-instant flake.
    #[traced_test]
    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_newer_no_drop_event() {
        // Same callsite-interest race as its paired test above. This one
        // asserts zero drops, so a lost race would make it pass for the
        // wrong reason; rebuild so the pair stays a faithful drop-vs-write
        // signal.
        tracing::callsite::rebuild_interest_cache();

        let mut disk = Instance::new("session", "/tmp/test");
        let older = Utc::now() - chrono::Duration::minutes(1);
        let newer = Utc::now();
        disk.last_accessed_at = Some(older);

        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(newer),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        logs_assert(|lines: &[&str]| match drop_log_count(lines) {
            0 => Ok(()),
            n => Err(format!("expected 0 drop events, got {n}")),
        });
        assert_eq!(disk.last_accessed_at, Some(newer));
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_newer_applies() {
        let mut disk = Instance::new("session", "/tmp/test");
        let older = Utc::now() - chrono::Duration::minutes(1);
        disk.last_accessed_at = Some(older);

        let newer = Utc::now();
        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(newer),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.last_accessed_at, Some(newer));
    }

    #[test]
    fn test_merge_passive_status_patch_last_accessed_at_boundary_disk_none_applies() {
        // disk.last_accessed_at == None means never touched, not "newer":
        // `is_some_and` short-circuits to false, so the patch always wins.
        let mut disk = Instance::new("session", "/tmp/test");
        disk.last_accessed_at = None;

        let ts = Utc::now();
        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: None,
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_twice_identical_is_idempotent() {
        let mut disk = Instance::new("session", "/tmp/test");
        let ts = Utc::now();
        let patch = PassiveStatusPatch {
            lifecycle_generation: 0,
            status: Status::Idle,
            idle_entered_at: Some(ts),
            last_accessed_at: Some(ts),
        };
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);
        disk.merge_passive_status_patch(&disk.id.clone(), &patch);

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(ts));
        assert_eq!(disk.last_accessed_at, Some(ts));
    }

    #[test]
    fn test_merge_passive_status_patch_twice_increasing_newer_wins() {
        let mut disk = Instance::new("session", "/tmp/test");
        let t0 = Utc::now() - chrono::Duration::minutes(1);
        let t1 = Utc::now();

        disk.merge_passive_status_patch(
            &disk.id.clone(),
            &PassiveStatusPatch {
                lifecycle_generation: 0,
                status: Status::Running,
                idle_entered_at: None,
                last_accessed_at: Some(t0),
            },
        );
        disk.merge_passive_status_patch(
            &disk.id.clone(),
            &PassiveStatusPatch {
                lifecycle_generation: 0,
                status: Status::Idle,
                idle_entered_at: Some(t1),
                last_accessed_at: Some(t1),
            },
        );

        assert_eq!(disk.status, Status::Idle);
        assert_eq!(disk.idle_entered_at, Some(t1));
        assert_eq!(disk.last_accessed_at, Some(t1));
    }

    #[test]
    fn test_merge_from_tui_copies_status_pipeline() {
        let mut stored = Instance::new("session", "/tmp/test");
        stored.status = Status::Idle;

        let mut src = Instance::new("session", "/tmp/test");
        src.id = stored.id.clone();
        src.status = Status::Running;
        src.idle_entered_at = Some(Utc::now());

        stored.merge_from_tui(&src);

        assert_eq!(stored.status, Status::Running);
        assert_eq!(stored.idle_entered_at, src.idle_entered_at);
    }

    #[test]
    fn test_merge_from_tui_takes_max_last_accessed() {
        let earlier = Utc::now() - chrono::Duration::minutes(5);
        let later = Utc::now();

        let mut stored = Instance::new("a", "/tmp/a");
        stored.last_accessed_at = Some(later);
        let mut src = Instance::new("a", "/tmp/a");
        src.id = stored.id.clone();
        src.last_accessed_at = Some(earlier);
        stored.merge_from_tui(&src);
        assert_eq!(
            stored.last_accessed_at,
            Some(later),
            "peer's freshest activity timestamp must survive a stale TUI src"
        );

        let mut stored = Instance::new("b", "/tmp/b");
        stored.last_accessed_at = Some(earlier);
        let mut src = Instance::new("b", "/tmp/b");
        src.id = stored.id.clone();
        src.last_accessed_at = Some(later);
        stored.merge_from_tui(&src);
        assert_eq!(stored.last_accessed_at, Some(later));
    }

    #[test]
    fn test_merge_from_tui_does_not_touch_user_action_fields() {
        let peer_archived = Some(Utc::now());
        let peer_favorited = Some(Utc::now() - chrono::Duration::minutes(2));
        let peer_snoozed = Some(Utc::now() + chrono::Duration::minutes(30));
        let peer_pinned = Some(Utc::now() - chrono::Duration::minutes(1));

        let mut stored = Instance::new("session", "/tmp/test");
        stored.archived_at = peer_archived;
        stored.favorited_at = peer_favorited;
        stored.snoozed_until = peer_snoozed;
        stored.pinned_at = peer_pinned;
        stored.title = "peer-renamed".to_string();
        stored.group_path = "peer/group".to_string();
        stored.agent_session_id = Some("daemon-sid".to_string());
        stored.notify_on_waiting = Some(true);
        stored.base_branch_override = Some("upstream/main".to_string());

        let mut src = Instance::new("session", "/tmp/test");
        src.id = stored.id.clone();
        src.archived_at = None;
        src.favorited_at = None;
        src.snoozed_until = None;
        src.pinned_at = None;
        src.title = "tui-stale".to_string();
        src.group_path = "tui/stale".to_string();
        src.agent_session_id = Some("tui-stale-sid".to_string());
        src.notify_on_waiting = Some(false);
        src.base_branch_override = None;

        stored.merge_from_tui(&src);

        assert_eq!(stored.archived_at, peer_archived);
        assert_eq!(stored.favorited_at, peer_favorited);
        assert_eq!(stored.snoozed_until, peer_snoozed);
        assert_eq!(stored.pinned_at, peer_pinned);
        assert_eq!(stored.title, "peer-renamed");
        assert_eq!(stored.group_path, "peer/group");
        assert_eq!(stored.agent_session_id.as_deref(), Some("daemon-sid"));
        assert_eq!(stored.notify_on_waiting, Some(true));
        assert_eq!(
            stored.base_branch_override.as_deref(),
            Some("upstream/main")
        );
    }

    #[test]
    fn test_merge_from_tui_syncs_launch_config_swap() {
        // The restart dialog mutates tool/command/extra_args in the TUI's
        // in-memory row. save() -> merge_from_tui must carry those onto disk,
        // otherwise reconcile_from_disk reverts the swap on the next launch and
        // the session respawns with its original tool.
        let mut stored = Instance::new("session", "/tmp/test");
        stored.tool = "claude".to_string();
        stored.command = String::new();
        stored.extra_args = String::new();

        let mut src = Instance::new("session", "/tmp/test");
        src.id = stored.id.clone();
        src.tool = "codex".to_string();
        src.command = "codex-wrapper".to_string();
        src.extra_args = "--foo".to_string();

        stored.merge_from_tui(&src);

        assert_eq!(stored.tool, "codex");
        assert_eq!(stored.command, "codex-wrapper");
        assert_eq!(stored.extra_args, "--foo");
    }

    #[test]
    fn test_merge_from_tui_preserves_immutable_identity() {
        let mut stored = Instance::new("session", "/tmp/test");
        let immutable_id = stored.id.clone();
        let immutable_path = stored.project_path.clone();
        let immutable_created = stored.created_at;

        let mut src = Instance::new("renamed", "/tmp/different");
        src.id = "different-id".to_string();

        stored.merge_from_tui(&src);

        assert_eq!(stored.id, immutable_id);
        assert_eq!(stored.project_path, immutable_path);
        assert_eq!(stored.created_at, immutable_created);
    }

    /// An engine swap parks the outgoing agent's conversation ids under its own
    /// name and picks the incoming agent's back up, so claude -> pi -> claude
    /// lands in the original Claude conversation instead of a third one. The
    /// per-agent selectors go; the approval posture stays (clearing it resolves
    /// the adapter's bypass mode on a `yolo_mode` row).
    ///
    /// Replaces a test that hand-assigned `agent_session_id = None` and then
    /// asserted it was None, which could not fail.
    #[test]
    fn swap_tool_parks_and_restores_per_tool_session_ids() {
        let mut inst = Instance::new("Test", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.agent_session_id = Some("claude-session-123".to_string());
        inst.acp_session_id = Some("acp-claude-1".to_string());
        inst.resume_probe_failed_sid = Some("claude-session-123".to_string());
        inst.acp_effort = Some("high".to_string());
        inst.agent_model = Some("claude-opus-4-7".to_string());
        inst.agent_name = Some("claude-code".to_string());
        inst.acp_mode_id = Some("plan".to_string());

        inst.swap_tool("pi");
        assert_eq!(inst.tool, "pi");
        assert_eq!(
            inst.agent_session_id, None,
            "a Claude sid would make pi launch with --resume <foreign-sid>"
        );
        assert_eq!(inst.acp_session_id, None);
        assert_eq!(inst.acp_effort, None);
        assert_eq!(inst.agent_model, None);
        assert_eq!(inst.agent_name, None);
        assert_eq!(inst.resume_probe_failed_sid, None);
        assert_eq!(inst.acp_mode_id.as_deref(), Some("plan"));

        // pi runs and captures a sid of its own, then the user swaps back.
        inst.agent_session_id = Some("pi-session-9".to_string());
        inst.swap_tool("claude");
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some("claude-session-123"),
            "swapping back must resume the parked Claude conversation"
        );
        assert_eq!(inst.acp_session_id.as_deref(), Some("acp-claude-1"));
        assert_eq!(
            inst.prior_tool_session_ids["pi"]
                .agent_session_id
                .as_deref(),
            Some("pi-session-9"),
            "pi's conversation is the parked one now"
        );
        assert!(
            !inst.prior_tool_session_ids.contains_key("claude"),
            "a restored entry is consumed, so a later swap cannot resurrect it"
        );

        // Same-tool call is a no-op: the caller applies the swap to the disk row
        // and the in-memory row independently, and the second must not re-park.
        inst.swap_tool("claude");
        assert_eq!(inst.agent_session_id.as_deref(), Some("claude-session-123"));
        assert!(!inst.prior_tool_session_ids.contains_key("claude"));
    }

    /// `swap_tool` re-resolves the alias for the incoming tool. The alias is
    /// per-tool, so carrying the outgoing tool's value forward aims every
    /// launch-time reader at the wrong built-in.
    #[test]
    fn swap_tool_reresolves_detect_as() {
        const PROFILE: &str = "detect-as-swap-test";
        let _registry = install_aliases(
            PROFILE,
            &[("claude-personal", "claude"), ("codex-personal", "codex")],
        );

        // (starting tool, stored alias, tool swapped to, expected alias)
        let cases = [
            // The reported row: created on a built-in (no alias to store),
            // then swapped onto a custom agent.
            ("claude", "", "claude-personal", "claude"),
            // Custom to custom: the outgoing alias is actively wrong, not
            // merely stale, so it cannot survive.
            ("codex-personal", "codex", "claude-personal", "claude"),
            // Custom back to a built-in: nothing to pin.
            ("claude-personal", "claude", "codex", ""),
        ];
        for (tool, detect_as, new_tool, expected) in cases {
            let mut inst = Instance::new("t", "/tmp/x");
            inst.source_profile = PROFILE.to_string();
            inst.tool = tool.to_string();
            inst.detect_as = detect_as.to_string();
            inst.swap_tool(new_tool);
            assert_eq!(inst.detect_as, expected, "{tool} -> {new_tool}");
        }
    }
}
