//! Status polling: pane capture, hook reconciliation, and the transition
//! bookkeeping that decides what a row displays.

use super::*;

impl Instance {
    /// Update status using pre-fetched pane metadata to avoid per-instance
    /// subprocess spawns. Falls back to subprocess calls if metadata is missing.
    ///
    /// Restamps `idle_entered_at` only when the detected status differs from
    /// [`Self::live_status_baseline`]. `last_accessed_at` is deliberately not
    /// written here (#3465): it is a user-gesture signal, and a poller stamp
    /// that advanced it on disk let `merge_user_action_diff`'s touched arm
    /// erase a concurrently archived row. The baseline invariant lives on the
    /// field itself; this method's job is the guard shape (baseline vs. newly
    /// detected). Every call re-seeds the baseline at exit, so the next call
    /// compares against a value this method itself wrote.
    pub fn update_status_with_metadata(
        &mut self,
        metadata: Option<&tmux::PaneMetadata>,
        resolved_name: Option<&str>,
    ) {
        let baseline = self.live_status_baseline;
        self.update_status_with_metadata_inner(metadata, resolved_name);
        if let Some(prev) = baseline {
            if prev != self.status {
                self.log_status_transition(prev);
                // last_accessed_at is deliberately NOT restamped here
                // (#3465): a passive advance reaches disk through
                // PassiveStatusPatch, and merge_user_action_diff's touched
                // arm reads any advance as a peer touch, wiping concurrent
                // archive/snooze/dormancy writes.
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

    /// One `info` line per observed status transition, carrying the evidence a
    /// wrong-state report needs: the hook file's value and age at the moment
    /// of the flip, and (for Claude) a content-free fingerprint of which pane
    /// markers were on screen. Intermittent status flakes can't be reproduced
    /// on demand, so this trail must land at the default log level; the
    /// per-rule detector traces stay at debug/trace for when a report narrows
    /// the hunt.
    ///
    /// Sessions are identified by the opaque instance id, not the title:
    /// smart-rename derives titles from the first prompt, so a title in an
    /// always-on log would leak conversation-derived text and break the
    /// content-free promise the pane fingerprint keeps. `aoe list` maps ids
    /// back to titles when correlating.
    ///
    /// The hook file is re-read here rather than threaded out of the detection
    /// path, so a value that changed in the microseconds since detection can
    /// disagree with the decision; the age field makes that visible. Costs one
    /// file stat, plus one pane capture for Claude, gated on an actual
    /// transition, so steady-state polling pays nothing.
    fn log_status_transition(&self, prev: Status) {
        // Resolved the same way the pane fallback resolves it, so the label and
        // the `pane=` fingerprint describe the detector that actually ran. The
        // ad-hoc `detect_as`-or-`tool` this used to do disagreed with the
        // detector whenever the stored alias was stale, which is exactly the
        // case a wrong-state report needs the log to be honest about.
        let detection_tool =
            tmux::status_rules::detection_tool(&self.source_profile, &self.tool, &self.detect_as);
        let hook = crate::hooks::read_hook_status(&self.id);
        let hook_age_ms = crate::hooks::read_hook_status_age(&self.id).map(|age| age.as_millis());
        if detection_tool == "claude" {
            let fingerprint = self
                .tmux_session()
                .ok()
                .and_then(|s| s.capture_pane(50).ok())
                .map(|pane| tmux::claude_pane_marker_fingerprint(&pane))
                .unwrap_or_else(|| "capture_failed".to_string());
            tracing::info!(target: "session.status_change",
                "{} [{}] {:?} -> {:?} (hook={:?} hook_age_ms={:?} pane={})",
                self.id, detection_tool, prev, self.status, hook, hook_age_ms, fingerprint
            );
        } else {
            tracing::info!(target: "session.status_change",
                "{} [{}] {:?} -> {:?} (hook={:?} hook_age_ms={:?})",
                self.id, detection_tool, prev, self.status, hook, hook_age_ms
            );
        }
    }

    /// Drop a [`TMUX_SESSION_GONE_ERROR`] left on a row that no longer has a
    /// tmux pane to speak for it, so the UI stops showing a message that cannot
    /// apply to it any more (a session converted to, or restarted in, the
    /// structured view).
    ///
    /// Shared by the structured short-circuit below and by the daemon poll
    /// loop's `skip_tmux_decision_for_structured`, which skips that
    /// short-circuit outright; one copy keeps the two from drifting.
    pub(crate) fn clear_stale_tmux_error(&mut self) {
        if self.last_error.as_deref() == Some(TMUX_SESSION_GONE_ERROR) {
            self.last_error = None;
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

        // Archived sessions have their tmux torn down on purpose (#1868), so
        // probing tmux here only ever produces a spurious "tmux session is
        // gone" Error transition (#2206). Short-circuit so the poller never
        // re-probes a row whose tmux is gone by design; this keeps
        // archive/unarchive status-preserving. Rows already persisted as Error
        // by a pre-fix build are cleaned up once by the v016 migration.
        if self.is_archived() {
            return;
        }

        // Acp-mode sessions are not backed by a tmux pane; the structured view
        // worker supervisor owns their lifecycle and emits typed health
        // events over the broadcast. Probing tmux here only ever produces
        // a spurious "tmux session is gone" Error transition.
        if self.is_structured() {
            self.clear_stale_tmux_error();
            if self.status == Status::Error {
                self.status = Status::Idle;
            }
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
                    self.status = Status::Error;
                    if self.last_error.is_none() {
                        self.last_error = Some(
                            "Could not reach tmux. Is tmux still running on the host?".to_string(),
                        );
                    }
                    self.last_error_check = Some(std::time::Instant::now());
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
                self.status = Status::Error;
                if self.last_error.is_none() {
                    self.last_error = Some(TMUX_SESSION_GONE_ERROR.to_string());
                }
                self.last_error_check = Some(std::time::Instant::now());
                return;
            }
            tmux::SessionExistence::Unknown => {
                // The tmux server itself was unreachable (stale socket,
                // refused connection), not a confirmed-absent session. This
                // is NOT evidence of anything on its own: a session that has
                // been confirmed alive rides out a bounded grace window
                // (absorbing a transient hiccup, the false-alarm bug this
                // branch exists to fix), but a session that has never once
                // been confirmed alive has nothing to "blip" from and gets a
                // much shorter one.
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
                self.status = Status::Error;
                if self.last_error.is_none() {
                    self.last_error = Some(TMUX_SERVER_UNREACHABLE_ERROR.to_string());
                }
                self.last_error_check = Some(std::time::Instant::now());
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

        let pane_cmd = metadata
            .and_then(|m| m.pane_current_command.clone())
            .or_else(|| tmux::utils::pane_current_command(session.name()));

        tracing::trace!(target: "session.store",
            "status '{}': exists=true, is_dead={}, pane_cmd={:?}, tool={}, cmd_override={}",
            self.title,
            is_dead,
            pane_cmd,
            self.tool,
            self.has_command_override()
        );

        // Two detection identities: hooks are installed for (and must be
        // interpreted by) the `agent_detect_as` alias when one is set, so
        // hook reconciliation keeps the alias identity. The pane fallback
        // below instead prefers the session's own configured status rules
        // over the alias.
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

        if let Some(hook_status) = crate::hooks::read_hook_status(&self.id) {
            tracing::trace!(target: "session.store",
                "status '{}': hook detected {:?}, is_dead={}",
                self.title,
                hook_status,
                is_dead
            );
            if is_dead {
                self.status = Status::Error;
                if self.last_error.is_none() {
                    let pane_content = session.capture_pane(20).unwrap_or_default();
                    self.last_error = Some(summarize_error_from_pane(&pane_content));
                }
            } else {
                // Three hook/pane mismatches need the pane captured and consulted:
                //
                // 1. Running hook, pane parked on a blocking prompt: Codex and
                //    Claude keep re-emitting running-mapped hooks while blocked,
                //    so a Running write can mean "still working" or "waiting on
                //    the user". Their reconcilers read the pane to tell which
                //    (Codex: plan/numbered prompts; Claude: tool-approval
                //    prompts, see #1913).
                // 2. Waiting hook gone stale: several agents write `waiting`
                //    directly when a prompt appears (Claude AskUserQuestion /
                //    permission prompt, Codex PermissionRequest, Cursor / Qwen /
                //    Gemini permission notifications). Esc-cancelling the prompt
                //    fires no completing hook, so the file sticks on `waiting`
                //    until the next turn (regression from #2937). Any such agent
                //    is reconciled against the pane by reconcile_waiting_hook.
                // 3. Idle hook on a session last observed Running/Waiting:
                //    Claude's `Notification(idle_prompt)` hook is
                //    fire-and-forget, so when a queued prompt submits at turn
                //    end its `idle` write can land after `UserPromptSubmit`'s
                //    `running`, showing Idle mid-turn until the first
                //    PreToolUse rewrites the file. The previous-status gate
                //    keeps parked sessions (the dominant steady state) from
                //    paying a capture per poll; see
                //    reconcile_claude_idle_hook_status.
                let reconciles_running = (hook_tool == "codex" || hook_tool == "claude")
                    && hook_status == Status::Running;
                let reconciles_waiting = hook_status == Status::Waiting;
                let reconciles_idle = hook_tool == "claude"
                    && hook_status == Status::Idle
                    && matches!(self.status, Status::Running | Status::Waiting);
                self.status = if reconciles_running || reconciles_waiting || reconciles_idle {
                    match session.capture_pane(50) {
                        Ok(pane_content) => {
                            if reconciles_waiting {
                                tmux::reconcile_waiting_hook(hook_tool, &pane_content)
                            } else if reconciles_idle {
                                tmux::reconcile_claude_idle_hook_status(&pane_content)
                            } else if hook_tool == "codex" {
                                tmux::reconcile_codex_hook_status(hook_status, &pane_content)
                            } else {
                                let running_age = crate::hooks::read_hook_status_age(&self.id);
                                tmux::reconcile_claude_hook_status(
                                    hook_status,
                                    &pane_content,
                                    running_age,
                                )
                            }
                        }
                        Err(e) => {
                            tracing::trace!(
                                "status '{}': {} hook fallback pane capture failed: {}",
                                self.title,
                                hook_tool,
                                e
                            );
                            hook_status
                        }
                    }
                } else {
                    hook_status
                };
                self.last_error = None;
            }
            return;
        }

        // Pane-fallback identity: the session's own configured status rules
        // outrank the `agent_detect_as` alias; without rules the alias applies.
        let pane_tool =
            tmux::status_rules::detection_tool(&self.source_profile, &self.tool, &self.detect_as);
        let pane_content = session.capture_pane(50).unwrap_or_default();
        let detected =
            tmux::detect_status_from_content_in(&self.source_profile, &pane_content, &pane_tool);
        tracing::trace!(target: "session.store",
            "status '{}': detected={:?}, cmd_override={}, custom_cmd={}",
            self.title,
            detected,
            self.has_command_override(),
            self.has_custom_command(),
        );
        let is_shell_stale = || {
            let expects = self.expects_shell();
            if expects {
                return false;
            }
            let shell_check = metadata
                .and_then(|m| {
                    m.pane_current_command.as_deref().map(|current_command| {
                        tmux::utils::is_pane_running_shell_command(
                            current_command,
                            m.pane_start_command_is_protected,
                        )
                    })
                })
                .unwrap_or_else(|| session.is_pane_running_shell());
            tracing::trace!(target: "session.store",
                "status '{}': is_shell_stale check: expects_shell={}, shell_check={}",
                self.title,
                expects,
                shell_check,
            );
            shell_check
        };
        let has_command_override = self.has_command_override();
        let shell_stale = if detected == Status::Idle && !has_command_override && !is_dead {
            is_shell_stale()
        } else {
            false
        };
        // A Claude pane with unsubmitted typed text in the input box can show
        // no running signal at all while a turn streams (typing suppresses the
        // `esc to interrupt` hint and prose streaming renders no spinner), and
        // that pane is identical to a parked one minus the completion line. In
        // the ambiguous state, hold an already-observed Running rather than
        // flap a working session to Idle; the completion line rendered at turn
        // end releases the hold on the next poll.
        let detected = if detected == Status::Idle
            && !shell_stale
            && !is_dead
            && self.status == Status::Running
            && pane_tool == "claude"
            && tmux::claude_pane_is_ambiguous_typed_prompt(&pane_content)
        {
            tracing::debug!(target: "session.store",
                "status '{}': holding Running over ambiguous typed-prompt Idle", self.title);
            Status::Running
        } else {
            detected
        };
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

    pub fn update_status(&mut self) {
        self.update_status_with_metadata(None, None);
    }

    /// Capture the session's window for the preview, with any panes the user
    /// split off composited in. `capture-pane` has no size parameters: the
    /// window is captured at its own dimensions.
    pub fn capture_output_composited(&self, lines: usize) -> Result<String> {
        self.tmux_session()?.capture_window_composited(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    #[test]
    fn test_archived_session_not_marked_error_when_tmux_gone() {
        // #2206: archiving kills the session's tmux on purpose. A subsequent
        // status poll must not flip the archived row to Error for the missing
        // tmux; the archived guard short-circuits, so an idle row stays Idle.
        // Red on the pre-fix tree, where the tmux probe stamps Error.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.update_status_with_metadata(None, None);
        assert_ne!(inst.status, Status::Error);
        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.last_error, None);
    }

    #[test]
    fn test_archived_session_preserves_genuine_error() {
        // #2206 regression guard (passes on both trees): the archived guard
        // never mutates status, so a genuinely errored session keeps its Error
        // state while archived. The legacy on-disk footprint is cleaned up by
        // the v016 migration, not by the poller.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    #[test]
    fn test_archived_unarchived_genuine_error_roundtrips() {
        // #2206: archive then unarchive must stay status-preserving for a real
        // failure. The archived guard leaves Error untouched; after unarchive
        // the tmux probe re-stamps Error and its is_none() guard preserves the
        // original message regardless of whether tmux is installed on the box.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        inst.update_status_with_metadata(None, None);
        inst.unarchive();
        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
    }

    /// Regression guard for the false-Error-latch bug: a confirmed-absent
    /// session (tmux server reachable, session missing from its list) must
    /// still latch `Status::Error` with `TMUX_SESSION_GONE_ERROR` exactly as
    /// before. Proves the `Unknown` fix did not soften the real-death case.
    #[test]
    #[serial_test::serial]
    fn test_confirmed_absent_session_still_latches_error() {
        let mut inst = Instance::new("test-absent", "/tmp/test-absent");
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;

        let guard = crate::tmux::SessionCacheGuard::capture();
        // Fresh cache, server reachable, but this instance's tmux session
        // name is not in it: a confirmed-absent session.
        guard.force_present(&["some_other_session"]);

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some(TMUX_SESSION_GONE_ERROR));
        assert!(inst.last_error_check.is_some());
    }

    /// the poller / serve / ps loops resolve the session's live tmux name
    /// once against the batch snapshot; the status probe must act on that name
    /// instead of resolving the id a second time from the (possibly stale)
    /// title. A live name the title could never derive proves which path ran:
    /// only the resolved-name path can confirm it present.
    #[test]
    #[serial_test::serial]
    fn update_status_probes_the_resolved_name_not_the_title() {
        let resolved = format!("{}live_elsewhere_00000000", crate::tmux::SESSION_PREFIX);

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[resolved.as_str()]);

        let mut inst = Instance::new("resolve-r2", "/tmp/resolve-r2");
        inst.status = Status::Running;
        inst.update_status_with_metadata_inner(None, Some(&resolved));
        assert!(
            inst.ever_confirmed_present,
            "the passed resolved name must be the one probed"
        );
        assert_ne!(inst.status, Status::Error);

        let mut untold = Instance::new("resolve-r2", "/tmp/resolve-r2");
        untold.status = Status::Running;
        untold.update_status_with_metadata_inner(None, None);
        assert_eq!(
            untold.status,
            Status::Error,
            "without the resolved name the title-derived name is absent from the cache"
        );
        assert_eq!(untold.last_error.as_deref(), Some(TMUX_SESSION_GONE_ERROR));
    }

    /// A tmux-server-unreachable probe (`SessionExistence::Unknown`) must not
    /// touch status, last_error, or last_error_check at all: a transient
    /// tmux hiccup must never look like every session died.
    #[test]
    #[serial_test::serial]
    fn test_unreachable_tmux_server_retains_running_status() {
        let mut inst = Instance::new("test-unknown", "/tmp/test-unknown");
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;

        let guard = crate::tmux::SessionCacheGuard::capture();
        // Fresh cache with no data: mirrors what `refresh_session_cache`
        // writes when `list-sessions` itself fails (stale socket, refused
        // connection), not a confirmed-absent session.
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.last_error, None);
        assert_eq!(inst.last_error_check, None);
    }

    /// Same `Unknown` retain-behavior, but starting from an already-set
    /// genuine `Status::Error`: an unreachable tmux server must not clear or
    /// overwrite a real prior failure either. "Retain" means untouched in
    /// both directions.
    #[test]
    #[serial_test::serial]
    fn test_unreachable_tmux_server_does_not_clear_existing_error() {
        let mut inst = Instance::new("test-unknown-error", "/tmp/test-unknown-error");
        inst.status = Status::Error;
        inst.last_error = Some("agent crashed".to_string());
        // None (rather than a stale Instant) so the 30s Error-recheck
        // throttle above this code path doesn't short-circuit before the
        // probe we're testing ever runs.
        inst.last_error_check = None;

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.last_error.as_deref(), Some("agent crashed"));
        assert_eq!(inst.last_error_check, None);
    }

    /// A session that has never been confirmed alive (`ever_confirmed_present`
    /// still `false`, e.g. `aoe add` without `--launch`) has nothing to
    /// "blip" from, so `Unknown` escalates to `Error` well before the long
    /// confirmed-present window; this is the case
    /// `web/tests/live/ensure-session-restart.spec.ts` depends on to see
    /// `Error` within its 10s wait.
    #[test]
    #[serial_test::serial]
    fn test_never_confirmed_present_unknown_escalates_after_fast_window() {
        let mut inst = Instance::new("test-never-present", "/tmp/test-never-present");
        inst.status = Status::Idle;
        inst.last_error = None;
        inst.last_error_check = None;
        assert!(!inst.ever_confirmed_present);
        inst.unknown_since = Some(
            std::time::Instant::now()
                - UNKNOWN_ERROR_WINDOW_NEVER_PRESENT
                - std::time::Duration::from_millis(1),
        );

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.last_error.as_deref(),
            Some(TMUX_SERVER_UNREACHABLE_ERROR)
        );
        assert!(inst.last_error_check.is_some());
    }

    /// The never-confirmed-present fast window must still absorb a fresh
    /// `Unknown` streak (elapsed just under the window), otherwise every
    /// freshly-added, not-yet-launched session would flap to `Error` on the
    /// very first couple of poll ticks before tmux even has a chance to
    /// answer.
    #[test]
    #[serial_test::serial]
    fn test_never_confirmed_present_unknown_retains_status_below_fast_window() {
        let mut inst = Instance::new("test-never-present-fresh", "/tmp/test-never-present-fresh");
        inst.status = Status::Idle;
        inst.last_error = None;
        inst.last_error_check = None;
        assert!(!inst.ever_confirmed_present);
        inst.unknown_since =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(500));

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.last_error, None);
        assert_eq!(inst.last_error_check, None);
    }

    /// The real production blip case: a session confirmed alive at some
    /// point must ride out an `Unknown` streak up to the long window,
    /// covering the ~11s max blip duration observed in production with
    /// margin, before ever latching `Error`.
    #[test]
    #[serial_test::serial]
    fn test_confirmed_present_unknown_retains_status_below_long_window() {
        let mut inst = Instance::new("test-confirmed-present", "/tmp/test-confirmed-present");
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;
        inst.ever_confirmed_present = true;
        // 11s: the max blip duration observed in production. Must not latch.
        inst.unknown_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(11));

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.last_error, None);
        assert_eq!(inst.last_error_check, None);
    }

    /// A session confirmed alive must still eventually latch `Error` once
    /// the tmux server has been unreachable past the long bounded window;
    /// the fix absorbs blips, it does not make a genuinely-dead server
    /// invisible forever.
    #[test]
    #[serial_test::serial]
    fn test_confirmed_present_unknown_escalates_after_long_window() {
        let mut inst = Instance::new(
            "test-confirmed-present-dead",
            "/tmp/test-confirmed-present-dead",
        );
        inst.status = Status::Running;
        inst.last_error = None;
        inst.last_error_check = None;
        inst.ever_confirmed_present = true;
        inst.unknown_since = Some(
            std::time::Instant::now()
                - UNKNOWN_ERROR_WINDOW_CONFIRMED_PRESENT
                - std::time::Duration::from_millis(1),
        );

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_unreachable();

        inst.update_status_with_metadata_inner(None, None);

        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.last_error.as_deref(),
            Some(TMUX_SERVER_UNREACHABLE_ERROR)
        );
        assert!(inst.last_error_check.is_some());
    }

    /// `Present` must clear a stale `unknown_since` and flip
    /// `ever_confirmed_present` on, so a session that recovers from a real
    /// outage is treated as confirmed-alive (long window) on its next
    /// `Unknown` streak rather than falling back to the never-confirmed-present
    /// fast window.
    #[test]
    #[serial_test::serial]
    fn test_present_clears_unknown_since_and_marks_ever_confirmed_present() {
        let mut inst = Instance::new("present-clears-unknown", "/tmp/present-clears-unknown");
        inst.status = Status::Idle;
        inst.unknown_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        assert!(!inst.ever_confirmed_present);
        let name = tmux::Session::generate_name(&inst.id, &inst.title);

        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[name.as_str()]);

        inst.update_status_with_metadata_inner(None, None);

        assert!(inst.ever_confirmed_present);
        assert_eq!(inst.unknown_since, None);
    }

    #[test]
    #[serial_test::serial]
    fn test_update_status_with_metadata_seeds_baseline_without_restamp() {
        // #2690: a session loaded fresh from disk (e.g. TUI relaunch, or
        // every tick of the daemon's status_poll_loop) has no live
        // observation history yet: `live_status_baseline` is `None`. The
        // very first status check must not treat a mismatch between the
        // disk-loaded `status` and the freshly detected status as a real
        // transition, or every reload would reset idle_entered_at/
        // last_accessed_at to `now`. Red on the pre-fix tree (which compares
        // against `self.status` directly and always restamps here, since no
        // real tmux session exists for this instance).
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = None;
        inst.status = Status::Starting;
        let stale_idle_entered_at = Some(Utc::now() - chrono::Duration::hours(2));
        let stale_last_accessed_at = Some(Utc::now() - chrono::Duration::hours(2));
        inst.idle_entered_at = stale_idle_entered_at;
        inst.last_accessed_at = stale_last_accessed_at;

        // Force detection to resolve to `Absent` -> Error deterministically:
        // a fresh cache snapshot that lists some other session but not this
        // instance's. Without this the outcome depends on whether an earlier
        // tmux-spawning test left a server reachable on the per-process
        // socket, making the test schedule-dependent and flaky (#2936).
        let _cache = force_session_absent();

        inst.update_status_with_metadata(None, None);

        // Detection confirms the session Absent, resolving to Error, which
        // differs from the stale disk `Starting`. That mismatch must NOT be
        // treated as a genuine transition.
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, stale_idle_entered_at,
            "first check after a fresh load must not clobber a stale-but-real idle_entered_at"
        );
        assert_eq!(
            inst.last_accessed_at, stale_last_accessed_at,
            "first check after a fresh load must not clobber a stale-but-real last_accessed_at"
        );
        assert_eq!(
            inst.live_status_baseline,
            Some(Status::Error),
            "the first check must seed the baseline for subsequent comparisons"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_update_status_with_metadata_keeps_last_accessed_at_on_transition() {
        // Once a live baseline is established, a real status change still
        // re-anchors idle_entered_at bookkeeping, but must NOT restamp
        // last_accessed_at (#3465): the field is a user-gesture signal, and
        // passive stamps reaching disk let merge_user_action_diff's touched
        // arm wipe concurrently archived rows.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = Some(Status::Idle);
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(Utc::now() - chrono::Duration::hours(2));
        let user_touch = Some(Utc::now() - chrono::Duration::hours(2));
        inst.last_accessed_at = user_touch;

        // Force detection to resolve to `Absent` -> Error deterministically
        // (see #2936; without this the outcome is schedule-dependent).
        let _cache = force_session_absent();

        inst.update_status_with_metadata(None, None);

        // Detection confirms the session Absent, resolving to Error: a
        // genuine transition away from the established Idle baseline.
        assert_eq!(inst.status, Status::Error);
        assert_eq!(inst.idle_entered_at, None);
        assert_eq!(
            inst.last_accessed_at, user_touch,
            "a passive transition must not fabricate a user-gesture stamp"
        );
        assert_eq!(inst.live_status_baseline, Some(Status::Error));
    }

    #[test]
    #[serial_test::serial]
    fn test_update_status_with_metadata_twice_same_status_never_restamps() {
        // Two consecutive calls that both detect the same status (session
        // confirmed Absent, so detection is deterministically Error) must
        // neither restamp: not the first (baseline already matches), and
        // not the second either.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.live_status_baseline = Some(Status::Error);
        inst.status = Status::Error;
        let sentinel_idle = Some(Utc::now() - chrono::Duration::hours(3));
        let sentinel_accessed = Some(Utc::now() - chrono::Duration::hours(3));
        inst.idle_entered_at = sentinel_idle;
        inst.last_accessed_at = sentinel_accessed;

        // Force detection to resolve to `Absent` -> Error deterministically
        // (see #2936; without this the outcome is schedule-dependent).
        let _cache = force_session_absent();

        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, sentinel_idle,
            "first call must not restamp"
        );
        assert_eq!(
            inst.last_accessed_at, sentinel_accessed,
            "first call must not restamp"
        );

        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.idle_entered_at, sentinel_idle,
            "second call must not restamp"
        );
        assert_eq!(
            inst.last_accessed_at, sentinel_accessed,
            "second call must not restamp"
        );
    }

    #[test]
    fn test_update_status_with_metadata_transitions_never_stamp_last_accessed_at() {
        // Two back-to-back genuine transitions update the idle_entered_at
        // bookkeeping and re-seed the baseline between calls, but neither
        // may touch last_accessed_at (#3465): passive stamps wiped
        // concurrent archives through merge_user_action_diff's touched arm.
        //
        // Archiving short-circuits update_status_with_metadata_inner before
        // it touches `status` (see the `is_archived()` guard), which lets
        // this test fully control the "detected" status for two
        // independent calls without a real tmux session.
        let mut inst = Instance::new("test", "/tmp/test");
        inst.archive();
        inst.live_status_baseline = Some(Status::Idle);
        inst.status = Status::Running;
        let user_touch = Some(Utc::now() - chrono::Duration::hours(2));
        inst.last_accessed_at = user_touch;

        inst.update_status_with_metadata(None, None);
        assert_eq!(
            inst.status,
            Status::Running,
            "archived guard preserves status"
        );
        assert_eq!(inst.idle_entered_at, None, "non-idle transition clears it");
        assert_eq!(inst.last_accessed_at, user_touch);
        assert_eq!(inst.live_status_baseline, Some(Status::Running));

        inst.status = Status::Idle;
        inst.update_status_with_metadata(None, None);
        assert_eq!(inst.status, Status::Idle);
        assert!(
            inst.idle_entered_at.is_some(),
            "entering Idle re-anchors idle_entered_at"
        );
        assert_eq!(inst.last_accessed_at, user_touch);
        assert_eq!(inst.live_status_baseline, Some(Status::Idle));
    }

    #[test]
    fn test_instance_new_seeds_live_status_baseline_none() {
        // #2690 follow-up. A freshly constructed Instance has no live
        // observation yet. Seeding `Some(Status::Idle)` here was the root
        // cause of the false restamp on the first poll after
        // `finalize_launch`: the baseline claimed "I saw Idle" while
        // `finalize_launch` (and other post-construction status writers)
        // advanced `status` to Starting without touching baseline, so the
        // wrapper's next call read `baseline=Some(Idle) != status=Starting`
        // and stamped `last_accessed_at` on a session no user ever
        // touched. Uniform `None` matches the disk-load path (which is
        // `None` because of `#[serde(skip)]`) so both paths seed on the
        // first poll rather than restamping.
        let inst = Instance::new("test", "/tmp/test");
        assert_eq!(inst.live_status_baseline, None);
    }

    #[test]
    fn test_first_poll_after_status_write_does_not_fabricate_last_accessed_at() {
        // #2690 follow-up regression lock. Reproduces the pre-fix bug:
        // `Instance::new` used to seed `live_status_baseline: Some(Idle)`,
        // then a post-construction status writer (like `finalize_launch`)
        // advanced `status` to Starting WITHOUT touching baseline. The
        // very first poll then read a stale baseline, treated the
        // detected-status mismatch as a "genuine transition", and stamped
        // `last_accessed_at` for a session the user never touched.
        //
        // Under the fix (`Instance::new` seeds `None`), the first poll
        // seeds baseline from the detected status and does NOT restamp;
        // `last_accessed_at` stays `None` for a truly untouched session.
        //
        // The assertion is guard-only: whatever `update_status_with_metadata_inner`
        // resolves `status` to (`Error` in the no-tmux path, could be a
        // different value if `_inner` grows a new branch), the wrapper's
        // `baseline.is_some_and(...)` guard at
        // [`Self::update_status_with_metadata`] short-circuits on
        // `baseline == None`, so no restamp path runs. A future refactor
        // of `_inner` cannot silently weaken the lock; only a change to
        // the wrapper's guard shape can.
        let mut inst = Instance::new("test", "/tmp/test");
        assert_eq!(inst.last_accessed_at, None, "fixture invariant");
        // Simulate any post-construction status writer, `finalize_launch`
        // being the canonical one (`src/session/instance/start.rs`).
        inst.status = Status::Starting;

        inst.update_status_with_metadata(None, None);

        assert_eq!(
            inst.last_accessed_at, None,
            "first poll must not fabricate a `last_accessed_at` on an untouched session"
        );
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

    /// End-to-end regression for #1913 through the real status pipeline.
    ///
    /// A sandboxed (or hook-equipped) Claude session reports `running` from
    /// its hook while the pane is actually parked on a tool-approval prompt:
    /// the `Notification` -> waiting write gets clobbered by a running-mapped
    /// hook that re-fires during concurrent turn activity, and Claude keeps
    /// its live spinner rendered below the prompt. Before the fix the pipeline
    /// trusted the hook's `running` and showed green; now it captures the pane
    /// and reconciles to Waiting.
    #[test]
    #[serial_test::serial]
    fn update_status_reconciles_running_hook_to_waiting_on_claude_approval_prompt() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut inst = Instance::new("aoe_test_1913_wait", "/tmp");
        assert_eq!(inst.tool, "claude");

        // Pane shows the approval prompt with the live spinner still active
        // below it, the exact shape from the issue screenshot. The spinner
        // line means the bare pane detector would say Running, so a green
        // reading here can only come from reconciliation doing its job.
        let pane = "  Bash command\n    \
touch /tmp/aoe_test_1913/marker.txt\n    Create marker file\n  \
Do you want to proceed?\n  \u{276f} 1. Yes\n    \
2. Yes, and always allow access to this project\n    3. No\n  \
Esc to cancel \u{b7} Tab to amend \u{b7} ctrl+e to explain\n\
\u{2736} Herding\u{2026} (53s \u{b7} \u{2193} 7.0k tokens)\n";
        let pane_file = std::env::temp_dir().join(format!("aoe_test_1913_{}.txt", inst.id));
        std::fs::write(&pane_file, pane).expect("write pane fixture");

        let session_name = tmux::Session::generate_name(&inst.id, &inst.title);
        let _guard = KillTmuxOnDrop(session_name.clone());
        // Single-quote the path so a temp dir with spaces or shell
        // metacharacters (e.g. macOS `$TMPDIR`) can't break the launch
        // command; embedded single quotes are closed/escaped/reopened.
        let quoted_pane_file =
            format!("'{}'", pane_file.to_string_lossy().replace('\'', r#"'\''"#));
        let launch = format!("cat {quoted_pane_file}; sleep 300");
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "40",
                &launch,
            ])
            .output()
            .expect("spawn tmux");
        assert!(
            created.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        // The clobbered hook state that produced the green row.
        use std::os::unix::fs::PermissionsExt;
        let base = crate::hooks::hook_base_path();
        if !base.exists() {
            std::fs::create_dir_all(&base).expect("create hook base dir");
        }
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("set hook base mode 0700");
        let dir = crate::hooks::hook_status_dir(&inst.id).expect("hook dir");
        std::fs::create_dir_all(&dir).expect("create hook dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("set hook instance mode 0700");
        std::fs::write(dir.join("status"), "running").expect("write status");
        assert_eq!(
            crate::hooks::read_hook_status(&inst.id),
            Some(Status::Running),
            "precondition: the raw hook signal is the Running that showed green"
        );

        // Wait for the pane to actually paint the cat output before the
        // authoritative read; a fixed sleep is flaky under parallel test load.
        let mut painted = false;
        for _ in 0..50 {
            let cap = crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", &session_name])
                .output();
            if let Ok(out) = cap {
                if String::from_utf8_lossy(&out.stdout).contains("Do you want to proceed?") {
                    painted = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(painted, "approval prompt never painted into the tmux pane");

        // `Session::exists()` reads a process-global 2s session cache that a
        // concurrent test may have snapshotted before this session existed,
        // which surfaces as a spurious Error (and the 30s error latch would
        // then pin it). Refresh from live tmux now that the pane is painted so
        // the single authoritative read sees a true existence result.
        crate::tmux::refresh_session_cache();
        inst.update_status();

        std::fs::remove_file(&pane_file).ok();
        crate::hooks::cleanup_hook_status_dir(&inst.id);

        assert_eq!(
            inst.status,
            Status::Waiting,
            "Claude blocked on an approval prompt must reconcile Running -> Waiting (#1913)"
        );
    }
}
