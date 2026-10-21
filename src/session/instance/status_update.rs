//! Status polling: pane capture, hook reconciliation, and the transition
//! bookkeeping that decides what a row displays.

use super::*;

/// Whether this poll can reuse the last verdict instead of capturing the pane.
fn skip_capture(
    activity: Option<i64>,
    last_activity: Option<i64>,
    last_capture_second: Option<i64>,
    has_hook: bool,
    pending: bool,
    observation_key: Option<tmux::ObservationKey>,
    last_observation_key: Option<tmux::ObservationKey>,
) -> bool {
    let Some(activity) = activity else {
        return false;
    };
    Some(activity) == last_activity
        && last_capture_second.is_some_and(|taken| taken > activity)
        && !has_hook
        && !pending
        && observation_key == last_observation_key
}

/// How long a `running` hook write keeps its authority for an agent that is still on a hand-written
/// detector.
const LEGACY_RUNNING_HOOK_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(900);

impl Instance {
    /// Update status using pre-fetched pane metadata to avoid per-instance subprocess spawns.
    pub fn update_status_with_metadata(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
    ) {
        self.poll_status(metadata, resolved_name, false);
    }

    /// Update status for a caller that observes this session exactly once.
    pub fn update_status_once(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
    ) {
        self.poll_status(metadata, resolved_name, true);
    }

    /// The body both entry points share. `single_poll` says no further
    /// observation is coming, which is what decides a held proposal.
    fn poll_status(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
        single_poll: bool,
    ) {
        if single_poll {
            // This observation decides, so only this observation may propose.
            self.detection.pending = None;
        }
        let baseline = self.live_status_baseline;
        self.update_status_with_metadata_inner(metadata, resolved_name);
        if single_poll {
            if let Some(pending) = self.detection.pending.take() {
                // Only a plain Idle is ever held, so there is no error explanation to keep or
                // derive.
                self.status = pending;
                self.last_error = None;
            }
        }
        if let Some(prev) = baseline {
            if prev != self.status {
                self.log_status_transition(prev);
                // last_accessed_at is deliberately NOT restamped here.
                let now = Utc::now();
                self.idle_entered_at = if self.status == Status::Idle {
                    Some(now)
                } else {
                    None
                };
            }
        }
        self.live_status_baseline = Some(self.status);
    }

    /// One `info` line per observed status transition, carrying the evidence a wrong-state report
    /// needs.
    fn log_status_transition(&self, prev: Status) {
        let detection_tool =
            tmux::status_rules::detection_tool(&self.source_profile, &self.tool, &self.detect_as);
        let hook = crate::hooks::read_hook_status(&self.id);
        let hook_age_ms = crate::hooks::read_hook_status_age(&self.id).map(|age| age.as_millis());
        tracing::info!(target: "session.status_change",
            "{} [{}] {:?} -> {:?} (hook={:?} hook_age_ms={:?} rule={})",
            self.id, detection_tool, prev, self.status, hook, hook_age_ms,
            self.detection.rule.unwrap_or("none")
        );
    }

    /// Drop a [`TMUX_SESSION_GONE_ERROR`] left on a row that no longer has a tmux pane to speak for
    /// it, so the UI stops showing a message that cannot apply to it any more (a session converted
    /// to, or restarted in, the structured view).
    pub(crate) fn clear_stale_tmux_error(&mut self) {
        if self.last_error.as_deref() == Some(TMUX_SESSION_GONE_ERROR) {
            self.last_error = None;
        }
    }

    /// Latch an Error on a tmux failure, keeping any explanation already recorded.
    fn latch_tmux_error(&mut self, message: &str) {
        self.status = Status::Error;
        if self.last_error.is_none() {
            self.last_error = Some(message.to_string());
        }
        self.last_error_check = Some(std::time::Instant::now());
    }

    /// Explain a pane failure from what the pane last printed, keeping any explanation already
    /// recorded.
    fn explain_error_from_pane(&mut self, session: &tmux::Session) {
        if self.last_error.is_none() {
            let pane_content = session.capture_pane(20).unwrap_or_default();
            self.last_error = Some(summarize_error_from_pane(&pane_content));
        }
    }

    pub(super) fn update_status_with_metadata_inner(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
    ) {
        if matches!(
            self.status,
            Status::Stopped | Status::Deleting | Status::Creating
        ) {
            return;
        }

        // Archived sessions have their tmux torn down on purpose, so probing tmux here only ever
        // produces a spurious "tmux session is gone" Error transition.
        if self.is_archived() {
            self.settle_archived_status();
            return;
        }

        // Acp-mode sessions are not backed by a tmux pane; the structured view worker supervisor
        // owns their lifecycle and emits typed health events over the broadcast.
        if self.is_structured() {
            self.clear_stale_tmux_error();
            if self.status == Status::Error {
                self.status = Status::Idle;
            }
            return;
        }

        // A peer process is launching this row and has not created its pane yet; the owner's
        // commit or failure write decides the outcome.
        if self.has_fresh_launch_reservation(Utc::now()) {
            self.status = Status::Starting;
            return;
        }

        if self.status == Status::Error && self.last_error.is_some() {
            if let Some(last_check) = self.last_error_check {
                if last_check.elapsed().as_secs() < 30 {
                    return;
                }
            }
        }

        if let Some(start_time) = self.last_start_time {
            if start_time.elapsed().as_secs() < 3 {
                self.status = Status::Starting;
                return;
            }
        }

        let session = match resolved_name {
            Some(name) => tmux::Session::from_name(name),
            None => match self.tmux_session() {
                Ok(s) => s,
                Err(_) => {
                    tracing::trace!(target: "session.store",
                        "status '{}': tmux_session() failed, setting Error",
                        self.title
                    );
                    self.latch_tmux_error(
                        "Could not reach tmux. Is tmux still running on the host?",
                    );
                    return;
                }
            },
        };

        match session.existence() {
            tmux::SessionExistence::Absent => {
                tracing::trace!(target: "session.store",
                    "status '{}': session.existence()=Absent (tmux name={}), setting Error",
                    self.title,
                    session.name()
                );
                self.unknown_since = None;
                self.latch_tmux_error(TMUX_SESSION_GONE_ERROR);
                return;
            }
            tmux::SessionExistence::Unknown => {
                // The tmux server itself was unreachable (stale socket, refused connection), not a
                // confirmed-absent session.
                let window = if self.ever_confirmed_present {
                    UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT
                } else {
                    UNKNOWN_ERROR_WINDOW_NEVER_PRESENT
                };
                let unknown_since = *self
                    .unknown_since
                    .get_or_insert_with(std::time::Instant::now);
                if unknown_since.elapsed() < window {
                    tracing::debug!(target: "session.store",
                        "status '{}': tmux server unreachable for {:?} (< {:?} window, ever_confirmed_present={}), retaining status {:?}",
                        self.title,
                        unknown_since.elapsed(),
                        window,
                        self.ever_confirmed_present,
                        self.status
                    );
                    return;
                }
                tracing::trace!(target: "session.store",
                    "status '{}': tmux server unreachable for {:?} (>= {:?} window, ever_confirmed_present={}), setting Error",
                    self.title,
                    unknown_since.elapsed(),
                    window,
                    self.ever_confirmed_present
                );
                self.latch_tmux_error(TMUX_SERVER_UNREACHABLE_ERROR);
                return;
            }
            tmux::SessionExistence::Present => {
                self.unknown_since = None;
                self.ever_confirmed_present = true;
            }
        }

        let is_dead = metadata
            .map(|m| m.pane_dead)
            .unwrap_or_else(|| session.is_pane_dead());

        let pane_cmd = metadata.and_then(|m| m.pane_current_command.as_deref());

        tracing::trace!(target: "session.store",
            "status '{}': exists=true, is_dead={}, pane_cmd={:?}, tool={}, cmd_override={}",
            self.title,
            is_dead,
            pane_cmd,
            self.tool,
            self.has_command_override()
        );
        let integration_configured = crate::plugin::agent_integration::configured(self);
        let integration_snapshot = integration_configured
            .then(|| crate::plugin::agent_integration::snapshot(self))
            .flatten();
        if let Some(status) = crate::plugin::agent_integration::session_status_override(
            integration_configured,
            is_dead,
            integration_snapshot
                .as_ref()
                .map(|snapshot| snapshot.status),
        ) {
            self.status = status;
            self.detection.pending = None;
            self.detection.rule = Some("agent_integration");
            if self.status == Status::Error {
                if is_dead {
                    self.explain_error_from_pane(&session);
                } else if self.last_error.is_none() {
                    self.last_error = Some(summarize_error_from_pane(
                        &session.capture_pane(20).unwrap_or_default(),
                    ));
                }
            } else {
                self.last_error = None;
            }
            return;
        }

        // Two detection identities: hooks are installed for (and must be interpreted by) the
        // `agent_detect_as` alias when one is set, so hook reconciliation keeps the alias identity.
        let hook_alias = tmux::status_rules::effective_detect_as(
            &self.source_profile,
            &self.tool,
            &self.detect_as,
        );
        let hook_tool: &str = if hook_alias.is_empty() {
            &self.tool
        } else {
            &hook_alias
        };
        let pane_tool =
            tmux::status_rules::detection_tool(&self.source_profile, &self.tool, &self.detect_as);

        let hook =
            crate::hooks::read_hook_status(&self.id).map(|status| tmux::detect::HookObservation {
                status,
                age: crate::hooks::read_hook_status_age(&self.id),
            });

        // A dead pane outranks every other signal, and only for a session that
        // reported hooks at all: a hookless agent's pane is allowed to end.
        if is_dead && hook.is_some() {
            self.status = Status::Error;
            self.explain_error_from_pane(&session);
            return;
        }

        if tmux::detect::has_manifest(hook_tool) {
            // Owned: both identities borrow `self`, which the manifest path
            // mutates.
            let agent = hook_tool.to_string();
            let rules_tool = pane_tool.to_string();
            self.update_status_from_manifest(
                &session,
                metadata,
                &agent,
                &rules_tool,
                hook,
                is_dead,
            );
            return;
        }

        // Agents still on hand-written detectors: the hook file decides.
        let hook =
            hook.filter(|_| !tmux::status_rules::has_rules(&self.source_profile, &pane_tool));
        if let Some(hook) = hook {
            let stale_running = hook.status == Status::Running
                && hook
                    .age
                    .is_some_and(|age| age >= LEGACY_RUNNING_HOOK_MAX_AGE);
            if !stale_running {
                self.status = hook.status;
                // An Error keeps its explanation, as the manifest path does.
                if hook.status == Status::Error {
                    self.explain_error_from_pane(&session);
                } else {
                    self.last_error = None;
                }
                return;
            }
            tracing::debug!(target: "session.store",
                "status '{}': {} `running` hook write is {:?} old, falling back to the pane",
                self.title, hook_tool, hook.age);
        }

        let pane_content = session.capture_pane(50).unwrap_or_default();
        let detected = if pane_tool == "prime-agent" {
            if is_dead {
                Status::Error
            } else {
                tmux::status_rules::detect(
                    &self.source_profile,
                    &pane_tool,
                    &crate::tmux::utils::strip_ansi(&pane_content),
                )
                .or_else(|| {
                    metadata
                        .and_then(|metadata| metadata.pane_pid)
                        .or_else(|| session.get_pane_pid())
                        .and_then(|pid| self.prime_live_status(pid))
                })
                .unwrap_or_else(|| {
                    if self.pane_is_stale_shell(metadata, &session) {
                        Status::Idle
                    } else {
                        Status::Unknown
                    }
                })
            }
        } else {
            tmux::detect_status_from_content_in(&self.source_profile, &pane_content, &pane_tool)
        };
        tracing::trace!(target: "session.store",
            "status '{}': detected={:?}, cmd_override={}, custom_cmd={}",
            self.title,
            detected,
            self.has_command_override(),
            self.has_custom_command(),
        );
        let has_command_override = self.has_command_override();
        let shell_stale = detected == Status::Idle
            && !has_command_override
            && !is_dead
            && self.pane_is_stale_shell(metadata, &session);
        self.status = resolve_detected_status(
            detected,
            is_dead,
            shell_stale,
            has_command_override,
            &pane_content,
            &self.tool,
        );

        tracing::trace!(target: "session.store", "status '{}': final={:?}", self.title, self.status);

        if self.status == Status::Error {
            if self.last_error.is_none() {
                self.last_error = Some(summarize_error_from_pane(&pane_content));
            }
        } else {
            self.last_error = None;
        }
    }

    /// Whether the pane is sitting on a bare shell rather than the agent it was launched with: the
    /// agent exited and left the pane's shell behind.
    fn pane_is_stale_shell(
        &self,
        metadata: Option<&tmux::PaneMetadata>,
        session: &tmux::Session,
    ) -> bool {
        if self.expects_shell() {
            return false;
        }
        metadata
            .and_then(|m| {
                m.pane_current_command.as_deref().map(|current_command| {
                    tmux::utils::is_pane_running_shell_command(
                        current_command,
                        m.pane_start_command_is_protected,
                    )
                })
            })
            .unwrap_or_else(|| session.is_pane_running_shell())
    }

    pub(crate) fn live_pane_owned(
        &self,
        session: &tmux::Session,
        metadata: Option<&tmux::PaneMetadata>,
        is_dead: bool,
    ) -> bool {
        if is_dead {
            return false;
        }
        if self.expects_shell() || self.has_command_override() {
            return true;
        }
        let fetched_command;
        let command = match metadata.and_then(|m| m.pane_current_command.as_deref()) {
            Some(command) => command,
            None => {
                fetched_command = tmux::utils::pane_current_command(session.name());
                let Some(command) = fetched_command.as_deref() else {
                    return true;
                };
                command
            }
        };
        !tmux::utils::is_shell_command(command)
            || metadata
                .map(|m| m.pane_start_command_is_protected)
                .unwrap_or_else(|| tmux::utils::pane_start_command_is_protected(session.name()))
    }

    /// Resolve status from the agent's detection manifest.
    fn update_status_from_manifest(
        &mut self,
        session: &tmux::Session,
        metadata: Option<&tmux::PaneMetadata>,
        agent: &str,
        rules_tool: &str,
        hook: Option<tmux::detect::HookObservation>,
        is_dead: bool,
    ) {
        let activity = metadata.and_then(|m| m.window_activity);
        let plan = tmux::DetectionPlan::for_manifest(&self.source_profile, rules_tool, agent)
            .expect("manifest was resolved before polling");
        let policy = plan.capture_policy();
        let fetched_title;
        let osc_title = match metadata.and_then(|m| m.pane_title.as_deref()) {
            Some(title) => title,
            None if policy.fetch_missing_title => {
                fetched_title = tmux::utils::pane_title(session.name()).unwrap_or_default();
                &fetched_title
            }
            None => "",
        };
        let live_owned =
            !policy.observe_ownership || self.live_pane_owned(session, metadata, is_dead);
        let observation = tmux::Observation::new(osc_title, hook, live_owned);
        let observation_key = plan.observation_key(&observation);
        let screen_unchanged = skip_capture(
            activity,
            self.detection.activity,
            self.detection.captured_at,
            hook.is_some(),
            self.detection.pending.is_some(),
            observation_key,
            self.detection.observation_key,
        );

        if screen_unchanged {
            // Nothing to re-decide, and nothing to re-derive from: the checks
            // below read the capture we deliberately did not take.
            self.detection.rule = Some("screen_unchanged");
            return;
        }

        // Stamped before the capture, never after.
        let captured_at = Utc::now().timestamp();
        let pane_content = session
            .capture_pane(policy.scope.lines())
            .unwrap_or_default();
        let clean = tmux::utils::strip_ansi(&pane_content);
        let detection = plan.detect(&clean, &observation);

        let Some(candidate) = detection.status else {
            // The screen is an agent-owned viewer; the last known status
            // stands rather than being overwritten by what a pager shows.
            self.detection.activity = activity;
            self.detection.captured_at = Some(captured_at);
            self.detection.rule = Some(detection.rule);
            self.detection.observation_key = observation_key;
            return;
        };

        let has_command_override = self.has_command_override();
        // A `waiting` hook write outlives an agent that exits from an open
        // prompt, so an inferred Waiting gets the same shell check as Idle.
        let shell_stale = (candidate == Status::Idle
            || (candidate == Status::Waiting && !detection.visible))
            && !has_command_override
            && !is_dead
            && if policy.observe_ownership {
                !live_owned
            } else {
                self.pane_is_stale_shell(metadata, session)
            };
        let candidate = resolve_detected_status(
            candidate,
            is_dead,
            shell_stale,
            has_command_override,
            &pane_content,
            &self.tool,
        );

        let confirmed = self.confirm_detection(candidate, detection.visible);
        if let Some(status) = confirmed {
            self.status = status;
            if status == Status::Error {
                if self.last_error.is_none() {
                    self.last_error = Some(
                        detection
                            .diagnostic
                            .map(pane_status::summarize_error_diagnostic)
                            .unwrap_or_else(|| summarize_error_from_pane(&pane_content)),
                    );
                }
            } else {
                self.last_error = None;
            }
        }
        self.detection.activity = activity;
        self.detection.captured_at = Some(captured_at);
        self.detection.rule = Some(detection.rule);
        self.detection.observation_key = observation_key;
        tracing::trace!(target: "session.store",
            "status '{}': manifest rule={} candidate={:?} visible={} -> {:?}",
            self.title, detection.rule, candidate, detection.visible, self.status);
    }

    /// Whether `candidate` may be published now.
    fn confirm_detection(&mut self, candidate: Status, visible: bool) -> Option<Status> {
        let needs_confirmation =
            self.status == Status::Running && candidate == Status::Idle && !visible;
        if !needs_confirmation {
            self.detection.pending = None;
            return Some(candidate);
        }
        match self.detection.pending {
            Some(pending) if pending == candidate => {
                self.detection.pending = None;
                Some(candidate)
            }
            _ => {
                self.detection.pending = Some(candidate);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;
    use std::time::{Duration, Instant};

    #[test]
    fn skip_capture_requires_an_unchanged_pane_captured_past_its_activity_second() {
        // (activity, last activity, last capture second, has hook, pending) -> skip
        for (args, skip) in [
            ((Some(100), Some(100), Some(101), false, false), true),
            // A pending proposal must be resolved by a capture, not skipped past.
            ((Some(100), Some(100), Some(101), false, true), false),
            ((Some(100), Some(100), Some(101), true, false), false),
            ((Some(101), Some(100), Some(102), false, false), false),
            ((None, None, Some(101), false, false), false),
            ((Some(100), None, Some(101), false, false), false),
            // A capture inside or before the activity second proves nothing (#3624).
            ((Some(100), Some(100), Some(100), false, false), false),
            ((Some(100), Some(100), Some(99), false, false), false),
            ((Some(100), Some(100), None, false, false), false),
        ] {
            let (activity, last, taken, hook, pending) = args;
            assert_eq!(
                skip_capture(activity, last, taken, hook, pending, None, None),
                skip,
                "{args:?}"
            );
        }
    }

    #[test]
    fn observation_changes_invalidate_cached_status_in_both_directions() {
        let plan =
            tmux::DetectionPlan::for_manifest("observation-cache-test", "omp", "omp").unwrap();
        let unmarked = plan.observation_key(&tmux::Observation::new("", None, true));
        let working = plan.observation_key(&tmux::Observation::new("π ⠋ Task", None, true));
        let next_frame =
            plan.observation_key(&tmux::Observation::new("π / Other title", None, true));
        let unowned = plan.observation_key(&tmux::Observation::new("π ⠋ Task", None, false));
        for (current, previous, skip) in [
            (unmarked, unmarked, true),
            (working, working, true),
            (next_frame, working, true),
            (unowned, unowned, true),
            (working, unmarked, false),
            (unmarked, working, false),
            (unowned, working, false),
            (working, unowned, false),
            (unowned, unmarked, false),
            (unmarked, unowned, false),
            (unmarked, None, false),
            (None, working, false),
        ] {
            assert_eq!(
                skip_capture(
                    Some(100),
                    Some(100),
                    Some(101),
                    false,
                    false,
                    current,
                    previous
                ),
                skip,
                "{previous:?} -> {current:?}",
            );
        }
    }

    #[test]
    fn archive_and_archived_poll_settle_live_status_and_keep_resting_ones() {
        type Settle = fn(&mut Instance);
        let entries: [(&str, Settle); 2] = [
            ("archive", |inst| inst.archive()),
            ("archived poll", |inst| {
                inst.archived_at = Some(Utc::now());
                inst.update_status_with_metadata(None, None);
            }),
        ];
        for (entry, settle) in entries {
            for (status, expected) in [
                (Status::Running, Status::Idle),
                (Status::Waiting, Status::Idle),
                (Status::Starting, Status::Idle),
                (Status::Idle, Status::Idle),
                (Status::Stopped, Status::Stopped),
                (Status::Error, Status::Error),
                (Status::Unknown, Status::Unknown),
            ] {
                let mut inst = Instance::new("test", "/tmp/test");
                inst.status = status;
                settle(&mut inst);
                assert!(inst.is_archived());
                assert_eq!(inst.status, expected, "{entry} {status:?}");
            }
        }

        // Archiving kills tmux on purpose: no Error for the missing session (#2206).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.update_status_with_metadata(None, None);
        assert_eq!((inst.status, inst.last_error.clone()), (Status::Idle, None));

        // A genuine failure survives archive and unarchive.
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        inst.unarchive();
        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    #[test]
    fn confirm_detection_holds_only_unwitnessed_idle() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Running;
        assert_eq!(inst.confirm_detection(Status::Idle, false), None);
        assert_eq!(inst.status, Status::Running);
        assert_eq!(
            inst.confirm_detection(Status::Idle, false),
            Some(Status::Idle)
        );

        // Chrome-witnessed idle and every other direction publish immediately.
        for (from, to, witnessed) in [
            (Status::Running, Status::Idle, true),
            (Status::Idle, Status::Running, false),
            (Status::Running, Status::Waiting, false),
        ] {
            inst.status = from;
            assert_eq!(
                inst.confirm_detection(to, witnessed),
                Some(to),
                "{from:?}->{to:?}"
            );
        }

        // A proposal that changes before it is confirmed starts over.
        inst.status = Status::Running;
        assert_eq!(inst.confirm_detection(Status::Idle, false), None);
        assert_eq!(
            inst.confirm_detection(Status::Waiting, false),
            Some(Status::Waiting)
        );
        assert!(inst.detection.pending.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn tmux_existence_decides_error_latching() {
        let never = UNKNOWN_ERROR_WINDOW_NEVER_PRESENT + Duration::from_millis(1);
        let confirmed = UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT + Duration::from_millis(1);
        // (label, reachable, start status, prior error, ever present, unknown streak,
        //  expected status, expected error)
        type StatusCase = (
            &'static str,
            bool,
            Status,
            Option<&'static str>,
            bool,
            Option<Duration>,
            Status,
            Option<&'static str>,
        );
        let cases: [StatusCase; 7] = [
            (
                "confirmed absent latches",
                true,
                Status::Running,
                None,
                false,
                None,
                Status::Error,
                Some(TMUX_SESSION_GONE_ERROR),
            ),
            (
                "unreachable retains running",
                false,
                Status::Running,
                None,
                false,
                None,
                Status::Running,
                None,
            ),
            (
                "unreachable keeps a real error",
                false,
                Status::Error,
                Some("agent crashed"),
                false,
                None,
                Status::Error,
                Some("agent crashed"),
            ),
            (
                "never present escalates past fast window",
                false,
                Status::Idle,
                None,
                false,
                Some(never),
                Status::Error,
                Some(TMUX_SERVER_UNREACHABLE_ERROR),
            ),
            (
                "never present absorbs a fresh streak",
                false,
                Status::Idle,
                None,
                false,
                Some(Duration::from_millis(500)),
                Status::Idle,
                None,
            ),
            (
                "confirmed rides out an 11s blip",
                false,
                Status::Running,
                None,
                true,
                Some(Duration::from_secs(11)),
                Status::Running,
                None,
            ),
            (
                "confirmed escalates past long window",
                false,
                Status::Running,
                None,
                true,
                Some(confirmed),
                Status::Error,
                Some(TMUX_SERVER_UNREACHABLE_ERROR),
            ),
        ];
        for (label, reachable, status, error, ever, streak, want_status, want_error) in cases {
            let mut inst = Instance::new("existence", "/tmp/existence");
            inst.status = status;
            inst.last_error = error.map(str::to_string);
            inst.ever_confirmed_present = ever;
            inst.unknown_since = streak.map(|age| Instant::now() - age);
            let guard = crate::tmux::SessionCacheGuard::capture();
            if reachable {
                guard.force_present(&["some_other_session"]);
            } else {
                guard.force_unreachable();
            }
            inst.update_status_with_metadata_inner(None, None);
            assert_eq!(inst.status, want_status, "{label}");
            assert_eq!(inst.last_error.as_deref(), want_error, "{label}");
            assert_eq!(
                inst.last_error_check.is_some(),
                want_status == Status::Error && status != Status::Error,
                "{label}"
            );
        }

        // Present clears a streak and marks the session confirmed alive.
        let mut inst = Instance::new("present-clears-unknown", "/tmp/present-clears-unknown");
        inst.unknown_since = Some(Instant::now() - Duration::from_secs(2));
        let name = tmux::Session::generate_name(&inst.id, &inst.title);
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[name.as_str()]);
        inst.update_status_with_metadata_inner(None, None);
        assert!(inst.ever_confirmed_present);
        assert_eq!(inst.unknown_since, None);
    }

    /// A row whose launch is still reserved by another process reads Starting, not a missing pane.
    #[test]
    #[serial_test::serial]
    fn reserved_launch_without_a_pane_reads_starting() {
        let _cache = force_session_absent();
        let now = Utc::now();
        for (op, at, want) in [
            (LifecycleOperation::Launch, now, Status::Starting),
            (
                LifecycleOperation::Launch,
                now - Instance::LIFECYCLE_RESERVATION_TTL,
                Status::Error,
            ),
            (LifecycleOperation::Stop, now, Status::Error),
        ] {
            let mut inst = Instance::new("reserved", "/tmp/reserved");
            inst.status = Status::Starting;
            inst.lifecycle_reservation = Some(LifecycleReservation {
                op,
                generation: inst.lifecycle_generation,
                at,
            });
            inst.update_status_with_metadata(None, None);
            assert_eq!(inst.status, want, "{op:?} at {at}");
        }
    }

    /// The poll loops resolve the live tmux name once against the batch snapshot.
    #[test]
    #[serial_test::serial]
    fn update_status_probes_the_resolved_name_not_the_title() {
        let resolved = format!("{}live_elsewhere_00000000", crate::tmux::SESSION_PREFIX);
        let guard = crate::tmux::SessionCacheGuard::capture();

        let mut inst = Instance::new("resolve-r2", "/tmp/resolve-r2");
        inst.status = Status::Running;
        guard.force_present(&[resolved.as_str()]);
        inst.update_status_with_metadata_inner(None, Some(&resolved));
        assert!(inst.ever_confirmed_present);
        assert_ne!(inst.status, Status::Error);

        let mut untold = Instance::new("resolve-r2", "/tmp/resolve-r2");
        untold.status = Status::Running;
        guard.force_present(&[resolved.as_str()]);
        untold.update_status_with_metadata_inner(None, None);
        assert_eq!(untold.status, Status::Error);
        assert_eq!(untold.last_error.as_deref(), Some(TMUX_SESSION_GONE_ERROR));
    }

    /// A passive poll re-anchors `idle_entered_at` only on a transition from a live baseline and
    /// never writes the user-gesture `last_accessed_at` (#2690, #3465).
    #[test]
    #[serial_test::serial]
    fn status_poll_bookkeeping_never_restamps_user_touch() {
        let stale = Some(Utc::now() - chrono::Duration::hours(2));
        let _cache = force_session_absent();
        // (baseline, disk status, expected idle_entered_at after the Error detection)
        for (baseline, status, idle_after) in [
            (None, Status::Starting, stale),
            (Some(Status::Idle), Status::Idle, None),
            (Some(Status::Error), Status::Error, stale),
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            assert_eq!(inst.live_status_baseline, None);
            inst.live_status_baseline = baseline;
            inst.status = status;
            inst.idle_entered_at = stale;
            inst.last_accessed_at = stale;
            for _ in 0..2 {
                inst.update_status_with_metadata(None, None);
                assert_eq!(inst.status, Status::Error);
                assert_eq!(inst.idle_entered_at, idle_after, "{baseline:?}");
                assert_eq!(inst.last_accessed_at, stale, "{baseline:?}");
                assert_eq!(inst.live_status_baseline, Some(Status::Error));
            }
        }

        let mut untouched = Instance::new("test", "/tmp/test");
        untouched.status = Status::Starting;
        untouched.update_status_with_metadata(None, None);
        assert_eq!(untouched.last_accessed_at, None);
    }

    #[test]
    fn archived_status_transitions_reanchor_idle_without_touching_last_accessed() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.live_status_baseline = Some(Status::Idle);
        inst.status = Status::Unknown;
        let user_touch = Some(Utc::now() - chrono::Duration::hours(2));
        inst.last_accessed_at = user_touch;

        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Unknown);
        assert_eq!(inst.idle_entered_at, None);
        assert_eq!(inst.last_accessed_at, user_touch);
        assert_eq!(inst.live_status_baseline, Some(Status::Unknown));

        inst.status = Status::Idle;
        inst.update_status_with_metadata(None, None);
        assert!(inst.idle_entered_at.is_some());
        assert_eq!(inst.last_accessed_at, user_touch);
        assert_eq!(inst.live_status_baseline, Some(Status::Idle));
    }

    struct KillTmuxOnDrop(String);

    impl Drop for KillTmuxOnDrop {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
        }
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn shell_quote(path: &std::path::Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', r#"'\''"#))
    }

    /// Start a detached 120x40 tmux session running `launch`; killed when the guard drops.
    fn spawn_pane(name: &str, launch: &str) -> KillTmuxOnDrop {
        let guard = KillTmuxOnDrop(name.to_string());
        let mut args = [
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "120",
            "-y",
            "40",
            launch,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        tmux::utils::append_pane_base_index_args(&mut args, name);
        let created = crate::tmux::tmux_command()
            .args(args)
            .output()
            .expect("spawn tmux");
        assert!(
            created.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        guard
    }

    fn wait_for_pane(name: &str, needle: &str) -> bool {
        for _ in 0..100 {
            let cap = crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", name])
                .output();
            if cap.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(needle)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    /// End-to-end regression for #1913 through the real status pipeline.
    #[test]
    #[serial_test::serial]
    fn update_status_reconciles_running_hook_to_waiting_on_claude_approval_prompt() {
        let home = crate::session::test_support::isolate_app_dir();
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut inst = Instance::new("aoe_test_1913_wait", "/tmp");
        inst.tool = "claude".into();
        inst.view = crate::session::View::Terminal;

        // The approval prompt remains visible above the live spinner.
        let pane = "  Bash command\n    \
touch /tmp/aoe_test_1913/marker.txt\n    Create marker file\n  \
Do you want to proceed?\n  \u{276f} 1. Yes\n    \
2. Yes, and always allow access to this project\n    3. No\n  \
Esc to cancel \u{b7} Tab to amend \u{b7} ctrl+e to explain\n\
\u{2736} Herding\u{2026} (53s \u{b7} \u{2193} 7.0k tokens)\n";
        let pane_file = home.path().join("pane.txt");
        std::fs::write(&pane_file, pane).expect("write pane fixture");

        let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
        let _guard = spawn_pane(
            &session_name,
            &format!("cat {}; sleep 300", shell_quote(&pane_file)),
        );

        // A stale running hook must not hide a visible approval prompt.
        write_hook_status(&inst.id, "running");
        assert_eq!(
            crate::hooks::read_hook_status(&inst.id),
            Some(Status::Running)
        );
        assert!(
            wait_for_pane(&session_name, "Do you want to proceed?"),
            "approval prompt never painted into the tmux pane"
        );

        crate::tmux::refresh_session_cache();
        inst.update_status_with_metadata(None, None);

        crate::hooks::cleanup_hook_status_dir(&inst.id);

        assert_eq!(
            inst.status,
            Status::Waiting,
            "Claude blocked on an approval prompt must reconcile Running -> Waiting (#1913)"
        );
    }

    /// Pane metadata for a live agent pane: not dead, running the agent itself, so neither the
    /// dead-pane branch nor the stale-shell check spawns tmux behind the test.
    fn agent_pane_metadata(command: &str, window_activity: Option<i64>) -> tmux::PaneMetadata {
        tmux::PaneMetadata {
            pane_dead: false,
            pane_current_command: Some(command.to_string()),
            pane_start_command_is_protected: false,
            pane_pid: None,
            pane_title: None,
            window_activity,
            window_size: None,
        }
    }

    fn write_hook_status(instance_id: &str, status: &str) {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::hooks::hook_base_path();
        if !base.exists() {
            std::fs::create_dir_all(&base).expect("create hook base dir");
        }
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("set hook base mode 0700");
        let dir = crate::hooks::hook_status_dir(instance_id).expect("hook dir");
        std::fs::create_dir_all(&dir).expect("create hook dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("set hook instance mode 0700");
        std::fs::write(dir.join("status"), status).expect("write status");
    }

    /// Install `rules` for `agent` under `profile` and return the registry
    /// guard that restores the profile's prior entries on drop.
    fn install_status_rules(
        profile: &str,
        agent: &str,
        rules: Vec<crate::session::config::StatusRule>,
    ) -> crate::tmux::status_rules::ProfileRegistryGuard {
        let guard = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
        let mut config = crate::session::Config::default();
        config
            .agents
            .entry(agent.to_string())
            .or_default()
            .status_rules = rules;
        crate::tmux::status_rules::install_from_config(profile, &config);
        guard
    }

    /// the hookless path's precedence is that a profile's own `[[agents.<name>.status_rules]]`
    /// decide, and the manifest path applies it.
    #[test]
    #[serial_test::serial]
    fn configured_rules_outrank_a_fresh_legacy_hook() {
        const PROFILE: &str = "legacy-hook-rules-precedence-test";
        let _registry = install_status_rules(
            PROFILE,
            "settl",
            vec![crate::session::config::StatusRule {
                status: crate::agents::HookStatus::Waiting,
                contains: Some("approve this?".to_string()),
                regex: None,
            }],
        );

        let mut inst = Instance::new("legacy-rules", "/tmp/legacy-rules");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "settl".to_string();
        inst.status = Status::Idle;
        assert!(
            !tmux::detect::has_manifest(&inst.tool),
            "fixture invariant: settl must still be on the legacy hook path"
        );
        write_hook_status(&inst.id, "running");

        let name = tmux::Session::generate_name(&inst.id, &inst.title);
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[name.as_str()]);

        let metadata = agent_pane_metadata("settl", None);
        inst.update_status_with_metadata_inner(Some(&metadata), None);
        crate::hooks::cleanup_hook_status_dir(&inst.id);

        assert_eq!(
            inst.status,
            Status::Idle,
            "configured rules must decide over a fresh `running` hook write"
        );
    }

    /// the legacy hook path cleared `last_error` unconditionally, so an `error` write landed a row
    /// on Error with no explanation to render and no `last_error` for the 30s error re-check
    /// throttle to key on.
    #[test]
    #[serial_test::serial]
    fn legacy_error_hook_keeps_an_explanation() {
        const PROFILE: &str = "legacy-hook-error-explanation-test";
        // An empty config under a profile of its own: no rules for settl, so
        // this exercises the hook short-circuit rather than the rules path.
        let _registry = install_status_rules(PROFILE, "settl", Vec::new());

        let mut inst = Instance::new("legacy-error", "/tmp/legacy-error");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "settl".to_string();
        inst.status = Status::Running;
        inst.last_error = None;
        write_hook_status(&inst.id, "error");

        let name = tmux::Session::generate_name(&inst.id, &inst.title);
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[name.as_str()]);
        let metadata = agent_pane_metadata("settl", None);

        inst.update_status_with_metadata_inner(Some(&metadata), None);
        assert_eq!(inst.status, Status::Error);
        assert!(
            inst.last_error.is_some(),
            "an Error hook must leave an explanation, which the error re-check \
             throttle also keys on"
        );

        // A standing explanation is the better one: it came from whatever
        // first diagnosed the failure.
        inst.last_error = Some("agent crashed".to_string());
        inst.last_error_check = None;
        inst.update_status_with_metadata_inner(Some(&metadata), None);
        crate::hooks::cleanup_hook_status_dir(&inst.id);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    /// A turn's running and idle frames can share tmux's activity second.
    #[test]
    #[serial_test::serial]
    fn idle_frame_sharing_an_activity_second_is_still_observed() {
        let home = crate::session::test_support::isolate_app_dir();
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut inst = Instance::new("aoe_test_3624_activity", "/tmp");
        inst.tool = "claude".into();
        inst.view = crate::session::View::Terminal;

        // The idle frame has no visible completion witness.
        let dir = home.path().join("frames");
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let running_file = dir.join("running.txt");
        let idle_file = dir.join("idle.txt");
        let marker = dir.join("marker");
        std::fs::write(&running_file, "\u{2736} Working\u{2026} (5s)\n").expect("write running");
        std::fs::write(&idle_file, format!("{}turn over\n", "\n".repeat(150))).expect("write idle");

        let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
        // The marker file is the test's clock: the pane holds the running frame until it appears.
        let launch = format!(
            "cat {}; until [ -f {} ]; do sleep 0.05; done; cat {}; sleep 300",
            shell_quote(&running_file),
            shell_quote(&marker),
            shell_quote(&idle_file),
        );
        let _guard = spawn_pane(&session_name, &launch);
        assert!(
            wait_for_pane(&session_name, "Working"),
            "running frame never painted"
        );

        let cache = crate::tmux::SessionCacheGuard::capture();
        cache.force_present(&[session_name.as_str()]);

        // Presence TTL is not the activity clock under test.
        let poll = |inst: &mut Instance, activity: Option<i64>| {
            cache.force_present(&[session_name.as_str()]);
            let metadata = agent_pane_metadata("claude", activity);
            inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        };

        // The activity value both frames share. tmux stamps it in whichever second the output
        // landed in, which is the second this first capture is taken in.
        poll(&mut inst, Some(0));
        assert_eq!(inst.status, Status::Running, "running frame must be seen");
        let shared = inst
            .detection
            .captured_at
            .expect("capture stamps its second");
        inst.detection.activity = Some(shared);

        std::fs::File::create(&marker).expect("touch marker");
        assert!(
            wait_for_pane(&session_name, "turn over"),
            "idle frame never painted"
        );

        poll(&mut inst, Some(shared));
        assert_eq!(
            inst.status,
            Status::Running,
            "an unwitnessed Idle waits one poll before it publishes"
        );
        assert_eq!(
            inst.detection.pending,
            Some(Status::Idle),
            "idle frame was not proposed; detection {:?}; pane {:?}",
            inst.detection,
            tmux::Session::from_name(&session_name).capture_pane(50)
        );

        poll(&mut inst, Some(shared));

        assert_eq!(
            inst.status,
            Status::Idle,
            "the confirming poll must publish the idle the final frame showed"
        );
    }

    /// One-shot readers must publish an unwitnessed Idle without a confirming poll.
    #[test]
    #[serial_test::serial]
    fn a_single_observation_publishes_an_unwitnessed_idle() {
        let home = crate::session::test_support::isolate_app_dir();
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut polled = Instance::new("aoe_test_3712_polled", "/tmp");
        polled.tool = "claude".into();
        polled.view = crate::session::View::Terminal;

        // A parked Claude prompt carrying half-typed text.
        let pane = "earlier output\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} half typed prompt\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle)\n";
        let pane_file = home.path().join("pane.txt");
        std::fs::write(&pane_file, pane).expect("write pane fixture");

        let session_name = tmux::Session::generate_name(&polled.id, &polled.title);
        let _guard = spawn_pane(
            &session_name,
            &format!("cat {}; sleep 300", shell_quote(&pane_file)),
        );
        let painted = wait_for_pane(&session_name, "half typed prompt");
        assert!(painted, "parked prompt never painted into the tmux pane");

        let cache = crate::tmux::SessionCacheGuard::capture();
        cache.force_present(&[session_name.as_str()]);
        let metadata = agent_pane_metadata("claude", None);

        // Both rows come off disk on `Running`, which is what the CLI reads.
        polled.status = Status::Running;
        cache.force_present(&[session_name.as_str()]);
        polled.update_status_with_metadata(Some(&metadata), Some(&session_name));
        assert_eq!(
            polled.status,
            Status::Running,
            "a repeating poller holds an unwitnessed Idle for the poll that agrees"
        );
        assert_eq!(polled.detection.pending, Some(Status::Idle));

        // A one-shot reader starts from a bare disk load: no proposal on
        // record, and none it can ever meet.
        let mut once = Instance::new("aoe_test_3712_once", "/tmp");
        once.tool = "claude".into();
        once.view = crate::session::View::Terminal;
        once.status = Status::Running;
        cache.force_present(&[session_name.as_str()]);
        once.update_status_once(Some(&metadata), Some(&session_name));
        assert_eq!(
            once.status,
            Status::Idle,
            "one observation is all this caller gets, so its proposal decides (#3712)"
        );
        assert_eq!(once.detection.pending, None);
    }

    #[test]
    #[serial_test::serial]
    fn omp_pinned_error_keeps_provider_diagnostic_above_plugin_rows() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        const PROVIDER_ERROR: &str = "Provider rejected this request: quota exceeded";
        const WRAPPED_CAPTION: &str = " Dismissed when you\n send your next\n message.";
        const MULTILINE_ERROR: &str =
            "Provider rejected this request:\n  quota exceeded\n\n  retry after reset\n  account remains blocked";
        for (message, caption, footer_rows) in [
            (
                PROVIDER_ERROR,
                " Dismissed when you send your next message.",
                12,
            ),
            (PROVIDER_ERROR, WRAPPED_CAPTION, 12),
            (PROVIDER_ERROR, WRAPPED_CAPTION, 16),
            (MULTILINE_ERROR, WRAPPED_CAPTION, 12),
            (
                PROVIDER_ERROR,
                " Dismissed when you send your next message.",
                0,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let pane_file = dir.path().join("pane.txt");
            let footer = if footer_rows == 0 {
                String::new()
            } else {
                format!("{}PLUGIN_READY\n", "PLUGIN\n".repeat(footer_rows - 1))
            };
            let pane = format!(
                "--------------------\n {message}\n{caption}\n--------------------\n\n\
                 ╭── pi > model ──╮\n╰────────────────╯\n{footer}"
            );
            std::fs::write(&pane_file, pane).unwrap();

            let mut inst = Instance::new("aoe_test_omp_error_diagnostic", "/tmp");
            inst.tool = "omp".to_owned();
            inst.source_profile = format!("omp-error-diagnostic-{}", inst.id);
            inst.status = Status::Running;
            let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
            let _guard = spawn_pane(
                &session_name,
                &format!("cat {}; exec sleep 300", shell_quote(&pane_file)),
            );
            let ready = if footer_rows == 0 {
                "╰────────────────╯"
            } else {
                "PLUGIN_READY"
            };
            assert!(
                wait_for_pane(&session_name, ready),
                "error pane never painted: footer_rows={footer_rows}, caption={caption:?}"
            );
            let cache = crate::tmux::SessionCacheGuard::capture();
            cache.force_present(&[session_name.as_str()]);
            let mut metadata = agent_pane_metadata("omp", None);
            metadata.pane_title = Some(String::new());

            inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));

            assert_eq!(
                inst.status,
                Status::Error,
                "footer_rows={footer_rows}, caption={caption:?}"
            );
            assert_eq!(
                inst.last_error.as_deref(),
                Some(message),
                "footer_rows={footer_rows}, caption={caption:?}"
            );
            inst.last_error = Some("Earlier failure remains latched".to_owned());
            inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
            assert_eq!(
                inst.last_error.as_deref(),
                Some("Earlier failure remains latched"),
                "an existing explanation survives another Error observation"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn omp_newer_activity_and_dialog_clear_retained_error_diagnostics() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }
        for (current, expected) in [
            (
                " ⠋ Auto server compaction…\n pi > model\n╰─",
                Status::Running,
            ),
            (
                "╭─ Tool approval ─────╮\n│ ❯ Approve │\n│   Deny │\n\
                 │ up/down navigate enter select esc cancel │\n╰─────────────────────╯",
                Status::Waiting,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let pane_file = dir.path().join("pane.txt");
            std::fs::write(
                &pane_file,
                format!(
                    "--------------------\n Provider request denied\n\
                     Dismissed when you send your next message.\n--------------------\n\n\
                     {current}\n{}PLUGIN_READY\n",
                    "PLUGIN\n".repeat(11)
                ),
            )
            .unwrap();
            let mut inst = Instance::new("aoe_test_omp_error_cleared", "/tmp");
            inst.tool = "omp".to_owned();
            inst.source_profile = format!("omp-error-cleared-{}", inst.id);
            inst.status = Status::Error;
            inst.last_error = Some("Earlier provider request denied".to_owned());
            let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
            let _guard = spawn_pane(
                &session_name,
                &format!("cat {}; exec sleep 300", shell_quote(&pane_file)),
            );
            assert!(wait_for_pane(&session_name, "PLUGIN_READY"));
            let cache = crate::tmux::SessionCacheGuard::capture();
            cache.force_present(&[session_name.as_str()]);
            let mut metadata = agent_pane_metadata("omp", None);
            metadata.pane_title = Some(String::new());

            inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));

            assert_eq!(inst.status, expected, "{current}");
            assert_eq!(inst.last_error, None, "{current}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn omp_title_only_transitions_and_ownership_loss_do_not_reuse_cached_status() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pane_file = dir.path().join("pane.txt");
        std::fs::write(&pane_file, "OMP_CAPTURE_READY\n$ ").unwrap();
        let mut inst = Instance::new("aoe_test_omp_cues", "/tmp");
        inst.tool = "omp".to_owned();
        inst.source_profile = format!("omp-cues-{}", inst.id);
        let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
        let _guard = spawn_pane(
            &session_name,
            &format!("cat {}; exec sleep 300", shell_quote(&pane_file)),
        );
        assert!(wait_for_pane(&session_name, "OMP_CAPTURE_READY"));
        assert!(crate::tmux::tmux_command()
            .args(["select-pane", "-t", &session_name, "-T", "π / Task"])
            .status()
            .unwrap()
            .success());
        let cache = crate::tmux::SessionCacheGuard::capture();
        cache.force_present(&[&session_name]);
        inst.update_status_once(None, Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Running,
            "metadata-free readers fetch the title"
        );

        let mut metadata = agent_pane_metadata("omp", Some(1));
        metadata.pane_title = Some("π: Task".to_owned());
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Running,
            "unwitnessed idle needs confirmation"
        );
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(inst.status, Status::Idle);

        metadata.pane_title = Some("π ⠋ Task".to_owned());
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Running,
            "title-only work invalidates cached idle"
        );
        metadata.pane_title = Some(String::new());
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(inst.status, Status::Running);
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Idle,
            "losing a title invalidates cached running"
        );

        metadata.pane_title = Some("π ⠋ Task".to_owned());
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(inst.status, Status::Running);
        metadata.pane_current_command = Some("bash".to_owned());
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Error,
            "a stale title cannot own a bare shell"
        );

        metadata.pane_start_command_is_protected = true;
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Running,
            "protected launch wrappers keep ownership"
        );
        metadata.pane_dead = true;
        inst.update_status_with_metadata_inner(Some(&metadata), Some(&session_name));
        assert_eq!(
            inst.status,
            Status::Error,
            "death outranks a protected wrapper and title"
        );
    }
}
