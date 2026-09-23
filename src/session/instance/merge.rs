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
        // A launch decided before a peer archived the row reports a pane
        // the archive tore down.
        if self.is_archived() {
            self.settle_archived_status();
        }
        self.sandbox_info = src.sandbox_info.clone();
        self.capture_started_at = src.capture_started_at;
    }

    /// Same fields as `merge_post_start`.
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
        let conversation_unchanged = before.conversation_state().matches(self);
        let marker_unchanged = self.resume_probe_failed_sid == before.resume_probe_failed_sid;

        if generation_can_merge {
            self.omp_capture_generation = src.omp_capture_generation.clone();
            if conversation_unchanged {
                self.adopt_conversation_state(src.conversation_state());
            }
        }
        if self.active_execution == src.active_execution {
            self.session_id_poller = src.session_id_poller.clone();
            self.session_id_poller_retry_after = src.session_id_poller_retry_after;
            if src.session_id_poller_is_running() {
                self.poller_repair.reset();
            }
        } else {
            src.stop_poller();
        }
        if generation_can_merge && marker_unchanged && self.agent_session_id == src.agent_session_id
        {
            self.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
        }
    }

    /// Carry runtime-only state across a storage reload without constructing a lifecycle snapshot
    /// from two different generations.
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
        // Reachability sentinels and detection bookkeeping are runtime-only just like poller
        // errors.
        self.ever_confirmed_present = previous.ever_confirmed_present;
        self.unknown_since = previous.unknown_since;
        self.detection = previous.detection;
        self.last_error = previous.last_error.clone();
        self.last_error_check = previous.last_error_check;
        self.last_start_time = previous.last_start_time;
        if self.active_execution == previous.active_execution {
            self.session_id_poller = previous.session_id_poller.clone();
            self.poller_repair = previous.poller_repair.clone();
            self.session_id_poller_retry_after = previous.session_id_poller_retry_after;
        } else {
            previous.stop_poller();
        }
        self.acp_load_session_capable = previous.acp_load_session_capable;
    }

    /// Carry every in-process field from a pre-move live row onto the committed disk-derived
    /// candidate published by `HomeView`.
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

    /// Splice TUI-mirrored, persisted fields from `src` onto `self`.
    pub fn merge_from_tui(&mut self, src: &Self) {
        if src.lifecycle_generation >= self.lifecycle_generation {
            self.lifecycle_generation = src.lifecycle_generation;
            self.status = src.status;
            self.last_accessed_at = self.last_accessed_at.max(src.last_accessed_at);
            self.idle_entered_at = src.idle_entered_at;
            // A snapshot taken before a peer archived the row carries a
            // pre-archive observation of a pane that no longer exists.
            if self.is_archived() {
                self.settle_archived_status();
            }
        }
        // Launch-config fields are TUI-authoritative and only mutated after creation by the restart
        // dialog (engine / command / args swap).
        self.tool = src.tool.clone();
        self.command = src.command.clone();
        self.extra_args = src.extra_args.clone();
    }

    /// Switch tools, parking completed conversations per tool.
    /// Pending forks retain their target for launch-time namespace validation.
    pub(crate) fn swap_tool(&mut self, new_tool: &str) {
        if new_tool == self.tool {
            return;
        }
        if !matches!(self.resume_intent, ResumeIntent::Fork { .. }) {
            let outgoing = PriorToolSession {
                agent_session_id: self.agent_session_id.take(),
                agent_session_binding: self.agent_session_binding.take(),
                pi_session_path: self.pi_session_path.take(),
                acp_session_id: self.acp_session_id.take(),
            };
            if !outgoing.is_empty() {
                self.prior_tool_session_ids
                    .insert(self.tool.clone(), outgoing);
            }
            let restored = self
                .prior_tool_session_ids
                .remove(new_tool)
                .unwrap_or_default();
            self.set_agent_conversation(
                restored.agent_session_id,
                restored.agent_session_binding,
                restored.pi_session_path,
            );
            self.acp_session_id = restored.acp_session_id;
            self.resume_intent = ResumeIntent::Default;
            self.resume_binding = None;
        }
        self.adopt_tool(new_tool);
        self.acp_load_session_capable = None;
        self.resume_probe_failed_sid = None;
        self.active_execution = None;
        self.acp_effort = None;
        self.agent_model = None;
        self.import_pending = None;
        self.fork_pending = None;
        self.agent_name = None;
    }

    /// Move this row to a different `tool` that runs the SAME agent on another
    /// account, keeping the conversation rather than parking it.
    ///
    /// Everything [`Self::swap_tool`] clears is cleared because it names
    /// something in the outgoing agent's namespace: a session id, a model, an
    /// effort vocabulary, a structured-view agent. None of that changes when
    /// only the account does, so all of it survives. What the incoming account
    /// lacks is the transcript itself, which
    /// [`crate::session::conversation_carry`] copies into its config root.
    ///
    /// The entry parked under `new_tool` by an earlier swap is dropped: the
    /// row's live conversation for that tool is now the carried one, and
    /// leaving the old id behind would let a later swap back restore a
    /// conversation this session has moved on from.
    ///
    /// Persistence has the same contract as [`Self::swap_tool`]: the caller
    /// writes the result to disk, or `reconcile_from_disk` reverts it.
    pub(crate) fn swap_account(&mut self, new_tool: &str) {
        if new_tool == self.tool {
            return;
        }
        self.adopt_tool(new_tool);
        self.prior_tool_session_ids.remove(new_tool);
        // The transcript the carry copies is what the previous failure was
        // missing, so the loop-breaker must not outlive the account it fired
        // on; the resume-probe cascade still catches a second failure.
        self.resume_probe_failed_sid = None;
        self.acp_load_session_capable = None;
    }

    /// Take on `new_tool`'s identity: the name plus the `agent_detect_as`
    /// alias resolved for it.
    ///
    /// The alias is resolved per-tool, so the outgoing tool's answer cannot
    /// survive: kept, it points `resolved_agent` at the wrong built-in
    /// outright (a `codex-personal` -> `claude-personal` swap would keep
    /// detecting as codex); cleared, the row lands in the same
    /// empty-`detect_as` state a session built before its tool joined
    /// `[session.agent_detect_as]` does. Re-resolve against the same
    /// process-global registry `effective_detect_as` reads, so this stays a
    /// lookup rather than a config load, and the row ends up exactly as if it
    /// had been built on the new tool.
    fn adopt_tool(&mut self, new_tool: &str) {
        self.tool = new_tool.to_string();
        self.detect_as =
            tmux::status_rules::effective_detect_as(&self.source_profile, new_tool, "")
                .into_owned();
    }

    /// Apply a passively-detected status transition to a disk row. Touches the same three fields as
    /// [`Self::merge_from_tui`] (`status`, `idle_entered_at`, `last_accessed_at`).
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
        // A patch decided from a pane observed before a concurrent archive landed is stale by
        // construction: the archive tore the tmux down.
        if self.is_archived() {
            self.settle_archived_status();
        }
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

    /// Merge the complete user-requested delta for a cross-profile move while preserving unrelated
    /// fields refreshed by a peer after `pre` was read. `account_swap` says the tool change keeps
    /// the same agent and changes only which account it runs as, so the conversation travels with
    /// the row instead of being parked (#4030). The caller classifies it, rather than this
    /// deciding for itself, so the row that lands matches the swap the restart already planned its
    /// transcript copy for.
    pub(crate) fn merge_profile_move_diff(&mut self, pre: &Self, post: &Self, account_swap: bool) {
        self.merge_user_action_diff(pre, post);
        if pre.tool != post.tool {
            // Apply the requested transition to the freshly locked disk row.
            if account_swap {
                self.swap_account(&post.tool);
            } else {
                self.swap_tool(&post.tool);
            }
        }
        splice(&mut self.command, &pre.command, &post.command);
        splice(&mut self.extra_args, &pre.extra_args, &post.extra_args);
    }

    /// Per-field-conditional splice: copy `post.X` onto `self.X` only when `pre.X != post.X`.
    pub fn merge_user_action_diff(&mut self, pre: &Self, post: &Self) {
        debug_assert_eq!(
            pre.source_profile, post.source_profile,
            "apply_user_action must not change source_profile; cross-profile moves go through mutate_instance"
        );
        splice(&mut self.title, &pre.title, &post.title);
        splice(&mut self.group_path, &pre.group_path, &post.group_path);
        splice(&mut self.archived_at, &pre.archived_at, &post.archived_at);
        splice(
            &mut self.favorited_at,
            &pre.favorited_at,
            &post.favorited_at,
        );
        splice(
            &mut self.snoozed_until,
            &pre.snoozed_until,
            &post.snoozed_until,
        );
        splice(&mut self.pinned_at, &pre.pinned_at, &post.pinned_at);
        splice(&mut self.trashed_at, &pre.trashed_at, &post.trashed_at);
        splice(
            &mut self.pre_trash_project_path,
            &pre.pre_trash_project_path,
            &post.pre_trash_project_path,
        );
        splice(&mut self.unread, &pre.unread, &post.unread);
        splice(
            &mut self.base_branch_override,
            &pre.base_branch_override,
            &post.base_branch_override,
        );
        splice(&mut self.color, &pre.color, &post.color);
        // Worktree workdir edit (move dir / rename branch) mutates these two.
        splice(
            &mut self.project_path,
            &pre.project_path,
            &post.project_path,
        );
        splice(
            &mut self.worktree_info,
            &pre.worktree_info,
            &post.worktree_info,
        );
        // `workspace_info` deliberately has NO arm.
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
        // touch_last_accessed(): clears archived + snoozed + idle-dormant. Does NOT clear favorite
        // or pin (both are explicit user-surfacing signals, not sink states).
        if touched {
            self.archived_at = None;
            self.snoozed_until = None;
            self.idle_dormant_since = None;
        }
        // Final-state invariant: archive is the strongest dismiss and wins over snooze.
        if self.archived_at.is_some() {
            self.snoozed_until = None;
        }
        // archive(): a row whose tmux archive tore down cannot hold a live-interaction status.
        if self.is_archived() {
            self.settle_archived_status();
        }
    }
}

/// Copy `post` onto `dst` only when the user action changed it (`pre != post`).
fn splice<T: PartialEq + Clone>(dst: &mut T, pre: &T, post: &T) {
    if pre != post {
        *dst = post.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    fn inst() -> Instance {
        Instance::new("s", "/tmp/x")
    }

    fn patch(
        status: Status,
        idle_entered_at: Option<DateTime<Utc>>,
        last_accessed_at: Option<DateTime<Utc>>,
    ) -> PassiveStatusPatch {
        PassiveStatusPatch {
            lifecycle_generation: 0,
            status,
            idle_entered_at,
            last_accessed_at,
        }
    }

    fn running_poller(id: &str) -> Arc<Mutex<SessionPoller>> {
        let mut poller = SessionPoller::new("omp-restarted".to_string());
        assert_eq!(
            poller.start(id.to_string(), Box::new(|| None), Box::new(|_| {}), None),
            crate::session::poller::PollerSpawn::Spawned
        );
        Arc::new(Mutex::new(poller))
    }

    fn stop(poller: &Arc<Mutex<SessionPoller>>) {
        poller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stop();
    }

    #[test]
    fn user_action_diff_propagates_set_and_clear() {
        type Action = fn(&mut Instance);
        type Check = fn(&Instance) -> bool;
        let cases: &[(&str, Action, Action, Check)] = &[
            (
                "unread",
                |i| i.unread = true,
                |i| i.unread = false,
                |i| i.unread,
            ),
            ("trash", |i| i.trash(), |i| i.untrash(), |i| i.is_trashed()),
        ];
        for (label, set, clear, is_set) in cases {
            let pre = inst();
            let mut post = pre.clone();
            set(&mut post);
            let mut disk = pre.clone();
            disk.merge_user_action_diff(&pre, &post);
            assert!(is_set(&disk), "{label} set");

            let pre = post.clone();
            let mut post = pre.clone();
            clear(&mut post);
            let mut disk = pre.clone();
            disk.merge_user_action_diff(&pre, &post);
            assert!(!is_set(&disk), "{label} cleared");
        }
    }

    #[test]
    fn archive_through_every_merge_settles_live_status() {
        for status in [Status::Running, Status::Waiting, Status::Starting] {
            // User-action splice has no `status` arm, so archive() must settle it on disk.
            let mut pre = inst();
            pre.status = status;
            let mut post = pre.clone();
            post.archive();
            let mut disk = pre.clone();
            disk.status = Status::Waiting;
            disk.merge_user_action_diff(&pre, &post);
            assert!(disk.archived_at.is_some());
            assert_eq!(disk.status, Status::Idle, "diff {status:?}");

            // A stale poll or TUI snapshot must not land a live status on an archived row.
            let mut disk = inst();
            disk.archived_at = Some(Utc::now());
            disk.merge_passive_status_patch(&disk.id.clone(), &patch(status, None, None));
            assert_eq!(disk.status, Status::Idle, "patch {status:?}");

            let mut stored = inst();
            stored.archived_at = Some(Utc::now());
            let mut src = stored.clone();
            src.status = status;
            stored.merge_from_tui(&src);
            assert_eq!(stored.status, Status::Idle, "tui {status:?}");
        }
        // A resting status survives archive; an unarchived row keeps its live status.
        let mut pre = inst();
        pre.status = Status::Error;
        let mut post = pre.clone();
        post.archive();
        let mut disk = pre.clone();
        disk.merge_user_action_diff(&pre, &post);
        assert_eq!(disk.status, Status::Error);

        let mut pre = inst();
        pre.status = Status::Waiting;
        let mut post = pre.clone();
        post.title = "renamed".to_string();
        let mut disk = pre.clone();
        disk.merge_user_action_diff(&pre, &post);
        assert_eq!(disk.status, Status::Waiting);

        let mut disk = inst();
        disk.merge_passive_status_patch(&disk.id.clone(), &patch(Status::Waiting, None, None));
        assert_eq!(disk.status, Status::Waiting);
    }

    #[test]
    fn merge_post_start_is_generation_ordered_and_keeps_peer_fields() {
        let floor = |secs| Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
        let mut live = inst();
        live.lifecycle_generation = 7;
        live.status = Status::Starting;
        live.idle_entered_at = Some(Utc::now() - chrono::Duration::minutes(5));
        live.last_error = Some("stale pane observation".to_string());
        live.capture_started_at = floor(1_000_000);
        let mut disk = live.clone();
        disk.lifecycle_generation = 8;
        disk.status = Status::Stopped;
        disk.idle_entered_at = None;
        disk.last_error = None;
        disk.capture_started_at = floor(2_000_000);
        live.merge_post_start(&disk);
        assert_eq!(live.lifecycle_generation, 8);
        assert_eq!(live.status, Status::Stopped);
        assert_eq!(
            (live.idle_entered_at, live.last_error.clone()),
            (None, None)
        );
        assert_eq!(live.capture_started_at, floor(2_000_000));

        let mut stored = inst();
        stored.archive();
        stored.agent_session_id = Some("daemon-sid".to_string());
        let mut working = inst();
        working.id = stored.id.clone();
        working.status = Status::Starting;
        stored.merge_post_start(&working);
        assert_eq!(stored.status, Status::Idle);
        assert!(stored.is_archived());
        assert_eq!(stored.agent_session_id.as_deref(), Some("daemon-sid"));
        working.status = Status::Waiting;
        stored.merge_post_restart(&working);
        assert_eq!(stored.status, Status::Idle);

        // A stale async or TUI result must not overwrite a newer lifecycle commit.
        stored.lifecycle_generation = 2;
        stored.status = Status::Stopped;
        stored.capture_started_at = floor(2_000_000);
        working.lifecycle_generation = 1;
        working.status = Status::Starting;
        working.capture_started_at = floor(1_000_000);
        stored.merge_post_start(&working);
        assert_eq!(stored.status, Status::Stopped);
        assert_eq!(stored.capture_started_at, floor(2_000_000));
        stored.merge_from_tui(&working);
        assert_eq!(stored.status, Status::Stopped);
    }

    #[test]
    fn runtime_reload_keeps_newer_disk_lifecycle_and_runtime_only_state() {
        let floor = |secs| Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
        let mut previous = inst();
        previous.lifecycle_generation = 3;
        previous.status = Status::Error;
        previous.idle_entered_at = Some(Utc::now());
        previous.last_error = Some(TMUX_SESSION_GONE_ERROR.to_string());
        previous.capture_started_at = floor(1_000_000);
        previous.acp_load_session_capable = Some(true);
        previous.ever_confirmed_present = true;
        let unknown_since = std::time::Instant::now() - std::time::Duration::from_secs(2);
        previous.unknown_since = Some(unknown_since);
        previous.detection = DetectionState {
            pending: Some(Status::Idle),
            ..Default::default()
        };

        let mut reloaded = inst();
        reloaded.id = previous.id.clone();
        reloaded.lifecycle_generation = 4;
        reloaded.status = Status::Stopped;
        reloaded.capture_started_at = floor(2_000_000);
        reloaded.merge_runtime_from_reload(&previous);
        assert_eq!(reloaded.lifecycle_generation, 4);
        assert_eq!(
            (reloaded.status, reloaded.idle_entered_at),
            (Status::Stopped, None)
        );
        assert_eq!(
            reloaded.last_error.as_deref(),
            Some(TMUX_SESSION_GONE_ERROR)
        );
        assert_eq!(reloaded.capture_started_at, floor(2_000_000));
        assert_eq!(reloaded.acp_load_session_capable, Some(true));
        assert!(reloaded.ever_confirmed_present);
        assert_eq!(reloaded.unknown_since, Some(unknown_since));
        // A reload between two polls must not drop a proposal awaiting confirmation (#3642).
        assert_eq!(reloaded.detection.pending, Some(Status::Idle));

        let mut same_generation = previous.clone();
        same_generation.capture_started_at = floor(2_000_000);
        same_generation.merge_runtime_from_reload(&previous);
        assert_eq!(same_generation.capture_started_at, floor(2_000_000));

        // Only a Purge reservation preserves the Deleting overlay.
        let mut deleting = inst();
        deleting.lifecycle_generation = 3;
        deleting.status = Status::Deleting;
        for (op, expected) in [
            (LifecycleOperation::Purge, Status::Deleting),
            (LifecycleOperation::Launch, Status::Stopped),
        ] {
            let mut reserved = deleting.clone();
            reserved.lifecycle_generation = 4;
            reserved.status = Status::Stopped;
            reserved.lifecycle_reservation = Some(LifecycleReservation {
                op,
                generation: 4,
                at: Utc::now(),
            });
            reserved.merge_runtime_from_reload(&deleting);
            assert_eq!(reserved.lifecycle_generation, 4);
            assert_eq!(reserved.status, expected, "{op:?}");
        }
    }

    #[test]
    fn merge_post_restart_keeps_peer_sid_and_matching_marker() {
        let mut stored = inst();
        stored.agent_session_id = Some("peer-fresh-sid".to_string());
        stored.resume_probe_failed_sid = Some("peer-fresh-sid".to_string());
        stored.snooze(15);
        let mut working = inst();
        working.id = stored.id.clone();
        working.status = Status::Starting;
        working.agent_session_id = Some("phase1-stale-sid".to_string());
        working.resume_probe_failed_sid = Some("phase1-stale-sid".to_string());
        stored.merge_post_restart(&working);
        assert_eq!(stored.status, Status::Starting);
        assert_eq!(stored.agent_session_id.as_deref(), Some("peer-fresh-sid"));
        assert_eq!(
            stored.resume_probe_failed_sid.as_deref(),
            Some("peer-fresh-sid")
        );
        assert!(stored.is_snoozed());

        let mut stored = inst();
        stored.agent_session_id = Some("failed-sid".to_string());
        let mut working = stored.clone();
        working.status = Status::Error;
        working.resume_probe_failed_sid = Some("failed-sid".to_string());
        stored.merge_post_restart(&working);
        assert_eq!(stored.status, Status::Error);
        assert_eq!(
            stored.resume_probe_failed_sid.as_deref(),
            Some("failed-sid")
        );
    }

    #[test]
    #[serial_test::serial]
    fn merge_post_restart_with_baseline_follows_omp_generation_and_poller() {
        let mut before = Instance::new("omp-session", "/tmp/test");
        before.agent_session_id = Some("old-sid".to_string());
        before.omp_capture_generation = Some("generation-a".to_string());
        let now = std::time::Instant::now();
        before.poller_repair.defer(now);
        before.poller_repair.defer(now);
        assert_eq!(before.poller_repair.deferrals(), 2);
        let mut restarted = before.clone();
        restarted.omp_capture_generation = Some("generation-b".to_string());
        restarted.poller_repair.reset();
        let restarted_poller = running_poller(&before.id);
        restarted.session_id_poller = Some(restarted_poller.clone());

        let mut live = before.clone();
        live.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(live.omp_capture_generation.as_deref(), Some("generation-b"));
        assert!(live.session_id_poller.is_some());
        assert_eq!(live.poller_repair.deferrals(), 0);

        let mut converged = before.clone();
        converged.agent_session_id = Some("peer-sid".to_string());
        converged.omp_capture_generation = Some("generation-b".to_string());
        converged.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(converged.agent_session_id.as_deref(), Some("peer-sid"));
        assert!(converged.session_id_poller.is_some());

        // A concurrent third generation keeps its own id but adopts the running poller.
        let mut peer_relaunched = before.clone();
        peer_relaunched.omp_capture_generation = Some("peer-generation".to_string());
        peer_relaunched.merge_post_restart_with_baseline(&before, &restarted);
        assert_eq!(
            peer_relaunched.omp_capture_generation.as_deref(),
            Some("peer-generation")
        );
        assert!(Arc::ptr_eq(
            peer_relaunched.session_id_poller.as_ref().unwrap(),
            &restarted_poller
        ));
        assert_eq!(peer_relaunched.poller_repair.deferrals(), 0);

        let mut not_started = restarted.clone();
        not_started.session_id_poller = None;
        let mut live = before.clone();
        live.merge_post_restart_with_baseline(&before, &not_started);
        assert_eq!(live.poller_repair.deferrals(), 2);
        stop(&restarted_poller);
    }

    #[test]
    fn user_action_diff_applies_triage_invariants_over_peer_writes() {
        type Action = fn(&mut Instance);
        type Check = fn(&Instance) -> bool;
        let archived: Check = |i| i.archived_at.is_some();
        let favorited: Check = |i| i.favorited_at.is_some();
        let pinned: Check = |i| i.pinned_at.is_some();
        let snoozed: Check = |i| i.snoozed_until.is_some();
        // (label, pre setup, tui action, peer action, [(check, expected)])
        let cases: &[(&str, Action, Action, Action, &[(Check, bool)])] = &[
            (
                "tui favorite beats peer archive",
                |_| {},
                |i| i.favorite(),
                |i| i.archive(),
                &[(favorited, true), (archived, false)],
            ),
            (
                "tui archive beats peer favorite",
                |_| {},
                |i| i.archive(),
                |i| i.favorite(),
                &[(archived, true), (favorited, false)],
            ),
            (
                "tui touch beats peer archive",
                |_| {},
                |i| i.touch_last_accessed(),
                |i| i.archive(),
                &[(archived, false)],
            ),
            (
                "peer touch beats tui archive",
                |i| i.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60)),
                |i| i.archive(),
                |i| i.touch_last_accessed(),
                &[(archived, false)],
            ),
            (
                "peer archive clears tui snooze",
                |_| {},
                |i| i.snooze(15),
                |i| i.archive(),
                &[(archived, true), (snoozed, false)],
            ),
            (
                "tui unfavorite keeps peer archive",
                |i| i.favorite(),
                |i| i.unfavorite(),
                |i| i.archive(),
                &[(favorited, false), (archived, true)],
            ),
            (
                "tui pin beats peer archive",
                |_| {},
                |i| i.pin(),
                |i| i.archive(),
                &[(pinned, true), (archived, false)],
            ),
            (
                "tui archive beats peer pin",
                |_| {},
                |i| i.archive(),
                |i| i.pin(),
                &[(archived, true), (pinned, false)],
            ),
            (
                "tui snooze beats peer pin",
                |_| {},
                |i| i.snooze(30),
                |i| i.pin(),
                &[(snoozed, true), (pinned, false)],
            ),
            (
                "peer touch keeps tui pin",
                |i| i.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60)),
                |i| i.pin(),
                |i| i.touch_last_accessed(),
                &[(pinned, true)],
            ),
        ];
        for (label, setup, tui, peer, checks) in cases {
            let mut pre = inst();
            setup(&mut pre);
            let mut post = pre.clone();
            tui(&mut post);
            let mut disk = pre.clone();
            peer(&mut disk);
            disk.merge_user_action_diff(&pre, &post);
            for (check, expected) in checks.iter() {
                assert_eq!(check(&disk), *expected, "{label}");
            }
        }
    }

    #[test]
    fn user_action_diff_preserves_runtime_status_and_peer_touch() {
        let mut pre = inst();
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
        assert_eq!(disk.status, Status::Waiting);
    }

    /// A passive transition must not read as a user touch and wipe a concurrent sink state (#3465).
    #[test]
    #[serial_test::serial]
    fn passive_transition_does_not_wipe_concurrent_sink_state() {
        type Action = fn(&mut Instance);
        type Check = fn(&Instance) -> bool;
        let cases: &[(&str, Action, Check)] = &[
            ("archived_at", |i| i.archive(), |i| i.archived_at.is_some()),
            (
                "snoozed_until",
                |i| i.snooze(15),
                |i| i.snoozed_until.is_some(),
            ),
            (
                "idle_dormant_since",
                |i| i.favorite(),
                |i| i.idle_dormant_since.is_some(),
            ),
        ];
        for (field, user_action, sink_present) in cases {
            let mut pre = inst();
            pre.live_status_baseline = Some(Status::Idle);
            pre.status = Status::Idle;
            pre.last_accessed_at = Some(Utc::now() - chrono::Duration::seconds(60));
            pre.idle_dormant_since = Some(Utc::now() - chrono::Duration::hours(5));
            let mut disk = pre.clone();
            let _cache = force_session_absent();
            disk.update_status_with_metadata(None, None);
            assert_eq!(disk.status, Status::Error);
            let mut post = pre.clone();
            user_action(&mut post);
            disk.merge_user_action_diff(&pre, &post);
            assert!(sink_present(&disk), "{field}");
        }
    }

    #[test]
    fn passive_status_patch_is_a_narrow_generation_guarded_splice() {
        let mut disk = inst();
        disk.status = Status::Running;
        disk.last_accessed_at = Some(Utc::now() - chrono::Duration::hours(1));
        disk.title = "peer-title".to_string();
        disk.group_path = "peer/group".to_string();
        disk.unread = true;
        disk.pinned_at = Some(Utc::now());
        let before = disk.clone();
        let now = Utc::now();
        let fresh = patch(Status::Idle, Some(now), Some(now));
        disk.merge_passive_status_patch(&disk.id.clone(), &fresh);
        assert_eq!(
            (disk.status, disk.idle_entered_at, disk.last_accessed_at),
            (Status::Idle, Some(now), Some(now))
        );
        assert_eq!(
            (&disk.title, &disk.group_path, disk.unread, disk.pinned_at),
            (
                &before.title,
                &before.group_path,
                before.unread,
                before.pinned_at
            )
        );
        // Idempotent when replayed.
        disk.merge_passive_status_patch(&disk.id.clone(), &fresh);
        assert_eq!(
            (disk.status, disk.last_accessed_at),
            (Status::Idle, Some(now))
        );

        disk.lifecycle_generation = 2;
        disk.status = Status::Stopped;
        let mut stale = patch(Status::Running, None, None);
        stale.lifecycle_generation = 1;
        disk.merge_passive_status_patch(&disk.id.clone(), &stale);
        assert_eq!(disk.status, Status::Stopped);
    }

    #[test]
    fn passive_status_patch_only_advances_last_accessed_at() {
        let ts = Utc::now();
        let older = ts - chrono::Duration::minutes(5);
        // (disk last_accessed_at, patch last_accessed_at, expected)
        for (disk_ts, patch_ts, expected) in [
            (None, None, None),
            (Some(ts), Some(older), Some(ts)),
            (Some(ts), Some(ts), Some(ts)),
            (Some(older), Some(ts), Some(ts)),
            (None, Some(ts), Some(ts)),
        ] {
            let mut disk = inst();
            disk.status = Status::Running;
            disk.last_accessed_at = disk_ts;
            disk.merge_passive_status_patch(
                &disk.id.clone(),
                &patch(Status::Idle, Some(older), patch_ts),
            );
            assert_eq!(disk.status, Status::Idle);
            assert_eq!(disk.idle_entered_at, Some(older));
            assert_eq!(
                disk.last_accessed_at, expected,
                "{disk_ts:?} <- {patch_ts:?}"
            );
        }
    }

    #[test]
    fn passive_status_patch_logs_only_a_dropped_last_accessed_at() {
        let logs = crate::session::test_support::LogCapture::start();
        let ts = Utc::now();
        let mut disk = inst();
        disk.last_accessed_at = Some(ts);
        // An equal timestamp is a no-op and says so; a newer one applies silently.
        disk.merge_passive_status_patch(&disk.id.clone(), &patch(Status::Idle, None, Some(ts)));
        let newer = ts + chrono::Duration::minutes(1);
        disk.merge_passive_status_patch(&disk.id.clone(), &patch(Status::Idle, None, Some(newer)));
        assert_eq!(disk.last_accessed_at, Some(newer));
        let logs = logs.contents();
        assert_eq!(
            logs.matches("dropped passive status patch's last_accessed_at as a no-op")
                .count(),
            1,
            "{logs}"
        );
    }

    #[test]
    fn merge_from_tui_copies_status_and_launch_config_only() {
        let earlier = Utc::now() - chrono::Duration::minutes(5);
        let later = Utc::now();
        let mut stored = inst();
        let (id, path, created) = (
            stored.id.clone(),
            stored.project_path.clone(),
            stored.created_at,
        );
        stored.last_accessed_at = Some(later);
        stored.archived_at = Some(later);
        stored.title = "peer-renamed".to_string();
        stored.agent_session_id = Some("daemon-sid".to_string());
        stored.notify_on_waiting = Some(true);
        stored.base_branch_override = Some("upstream/main".to_string());
        stored.tool = "claude".to_string();

        let mut src = Instance::new("tui-stale", "/tmp/different");
        src.id = "different-id".to_string();
        src.status = Status::Error;
        src.idle_entered_at = Some(later);
        src.last_accessed_at = Some(earlier);
        src.agent_session_id = Some("tui-stale-sid".to_string());
        src.notify_on_waiting = Some(false);
        src.tool = "codex".to_string();
        src.command = "codex-wrapper".to_string();
        src.extra_args = "--foo".to_string();
        stored.merge_from_tui(&src);

        assert_eq!(
            (stored.status, stored.idle_entered_at),
            (Status::Error, Some(later))
        );
        assert_eq!(stored.last_accessed_at, Some(later));
        assert_eq!(
            (
                stored.tool.as_str(),
                stored.command.as_str(),
                stored.extra_args.as_str()
            ),
            ("codex", "codex-wrapper", "--foo")
        );
        assert_eq!(
            (stored.id, stored.project_path, stored.created_at),
            (id, path, created)
        );
        assert_eq!(stored.archived_at, Some(later));
        assert_eq!(stored.title, "peer-renamed");
        assert_eq!(stored.agent_session_id.as_deref(), Some("daemon-sid"));
        assert_eq!(stored.notify_on_waiting, Some(true));
        assert_eq!(
            stored.base_branch_override.as_deref(),
            Some("upstream/main")
        );

        let mut stale = inst();
        stale.last_accessed_at = Some(earlier);
        let mut src = stale.clone();
        src.last_accessed_at = Some(later);
        stale.merge_from_tui(&src);
        assert_eq!(stale.last_accessed_at, Some(later));
    }

    /// claude -> pi -> claude resumes the parked Claude conversation, not a third one.
    #[test]
    fn swap_tool_parks_and_restores_per_tool_session_ids() {
        let mut inst = tool_instance("claude", "/home/user/project");
        inst.agent_session_id = Some("claude-session-123".to_string());
        inst.acp_session_id = Some("acp-claude-1".to_string());
        inst.acp_load_session_capable = Some(true);
        inst.resume_probe_failed_sid = Some("claude-session-123".to_string());
        inst.acp_effort = Some("high".to_string());
        inst.agent_model = Some("claude-opus-4-7".to_string());
        inst.agent_name = Some("claude-code".to_string());
        inst.acp_mode_id = Some("plan".to_string());

        inst.swap_tool("pi");
        assert_eq!(inst.tool, "pi");
        assert_eq!(
            (inst.agent_session_id.clone(), inst.acp_session_id.clone()),
            (None, None)
        );
        assert_eq!(inst.acp_load_session_capable, None);
        assert_eq!(
            (
                inst.acp_effort.clone(),
                inst.agent_model.clone(),
                inst.agent_name.clone()
            ),
            (None, None, None)
        );
        assert_eq!(inst.resume_probe_failed_sid, None);
        assert_eq!(inst.acp_mode_id.as_deref(), Some("plan"));

        inst.agent_session_id = Some("pi-session-9".to_string());
        inst.acp_load_session_capable = Some(false);
        inst.swap_tool("claude");
        assert_eq!(inst.agent_session_id.as_deref(), Some("claude-session-123"));
        assert_eq!(inst.acp_session_id.as_deref(), Some("acp-claude-1"));
        assert_eq!(inst.acp_load_session_capable, None);
        assert_eq!(
            inst.prior_tool_session_ids["pi"]
                .agent_session_id
                .as_deref(),
            Some("pi-session-9")
        );
        assert!(!inst.prior_tool_session_ids.contains_key("claude"));

        // Same-tool swap is a no-op, so disk and memory rows can both apply it.
        inst.swap_tool("claude");
        assert_eq!(inst.agent_session_id.as_deref(), Some("claude-session-123"));
        assert!(!inst.prior_tool_session_ids.contains_key("claude"));
    }

    /// The cross-profile move re-applies the swap to the freshly locked disk
    /// row, so it has to be told which swap the restart classified. Left to
    /// park, an account swap lands the moved row with no session id and the
    /// transcript the carry copied is orphaned (#4030).
    #[test]
    fn merge_profile_move_diff_carries_the_conversation_on_an_account_swap() {
        const PROFILE: &str = "profile-move-account-swap-test";
        let _registry = install_aliases(PROFILE, &[("claude-1", "claude"), ("claude-2", "claude")]);

        // (account swap, sid on the moved row, sid parked under the old tool)
        let cases = [
            (true, Some("durable-sid"), None),
            (false, None, Some("durable-sid")),
        ];
        for (account_swap, expected_live, expected_parked) in cases {
            let mut locked = Instance::new("t", "/tmp/x");
            locked.source_profile = PROFILE.to_string();
            locked.tool = "claude-1".to_string();
            locked.detect_as = "claude".to_string();
            locked.agent_session_id = Some("durable-sid".to_string());

            let mut pre = locked.clone();
            pre.agent_session_id = Some("stale-snapshot-sid".to_string());
            let mut post = pre.clone();
            post.tool = "claude-2".to_string();

            locked.merge_profile_move_diff(&pre, &post, account_swap);

            assert_eq!(locked.tool, "claude-2", "account_swap={account_swap}");
            assert_eq!(
                locked.agent_session_id.as_deref(),
                expected_live,
                "account_swap={account_swap}: the locked row's own id is the durable one"
            );
            assert_eq!(
                locked
                    .prior_tool_session_ids
                    .get("claude-1")
                    .and_then(|parked| parked.agent_session_id.as_deref()),
                expected_parked,
                "account_swap={account_swap}"
            );
        }
    }

    #[test]
    fn swap_account_keeps_the_conversation_and_drops_the_parked_one() {
        const PROFILE: &str = "account-swap-test";
        let _registry = install_aliases(PROFILE, &[("claude-1", "claude"), ("claude-2", "claude")]);

        let mut inst = Instance::new("Test", "/home/user/project");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "claude-1".to_string();
        inst.detect_as = "claude".to_string();
        inst.agent_session_id = Some("claude-session-123".to_string());
        inst.acp_session_id = Some("acp-claude-1".to_string());
        inst.resume_intent = ResumeIntent::Use("claude-session-123".to_string());
        inst.resume_probe_failed_sid = Some("claude-session-123".to_string());
        inst.agent_model = Some("claude-opus-4-7".to_string());
        inst.acp_effort = Some("high".to_string());
        inst.agent_name = Some("claude-code".to_string());
        inst.prior_tool_session_ids.insert(
            "claude-2".to_string(),
            PriorToolSession {
                agent_session_id: Some("stale-on-the-other-account".to_string()),
                acp_session_id: None,
                agent_session_binding: None,
                pi_session_path: None,
            },
        );

        inst.swap_account("claude-2");

        assert_eq!(inst.tool, "claude-2");
        assert_eq!(inst.detect_as, "claude");
        assert_eq!(inst.agent_session_id.as_deref(), Some("claude-session-123"));
        assert_eq!(inst.acp_session_id.as_deref(), Some("acp-claude-1"));
        assert_eq!(
            inst.resume_intent,
            ResumeIntent::Use("claude-session-123".to_string()),
            "the pinned id names the same agent's namespace, so it survives"
        );
        assert_eq!(inst.agent_model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(inst.acp_effort.as_deref(), Some("high"));
        assert_eq!(inst.agent_name.as_deref(), Some("claude-code"));
        assert_eq!(
            inst.resume_probe_failed_sid, None,
            "the carried transcript is what the failed probe was missing"
        );
        assert!(
            !inst.prior_tool_session_ids.contains_key("claude-2"),
            "the carried conversation is this tool's live one now"
        );
        assert!(
            !inst.prior_tool_session_ids.contains_key("claude-1"),
            "nothing is parked: the conversation moved rather than stayed behind"
        );

        // Same-tool call is a no-op: the caller applies the swap to the disk
        // row and the in-memory row independently.
        inst.prior_tool_session_ids.insert(
            "claude-2".to_string(),
            PriorToolSession {
                agent_session_id: Some("keep-me".to_string()),
                acp_session_id: None,
                agent_session_binding: None,
                pi_session_path: None,
            },
        );
        inst.swap_account("claude-2");
        assert!(inst.prior_tool_session_ids.contains_key("claude-2"));
    }

    #[test]
    fn swap_tool_reresolves_detect_as() {
        const PROFILE: &str = "detect-as-swap-test";
        let _registry = install_aliases(
            PROFILE,
            &[("claude-personal", "claude"), ("codex-personal", "codex")],
        );
        for (tool, detect_as, new_tool, expected) in [
            ("claude", "", "claude-personal", "claude"),
            ("codex-personal", "codex", "claude-personal", "claude"),
            ("claude-personal", "claude", "codex", ""),
        ] {
            let mut inst = tool_instance(tool, "/tmp/x");
            inst.source_profile = PROFILE.to_string();
            inst.detect_as = detect_as.to_string();
            inst.swap_tool(new_tool);
            assert_eq!(inst.detect_as, expected, "{tool} -> {new_tool}");
        }
    }
}
