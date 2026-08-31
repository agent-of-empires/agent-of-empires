//! Restart and resume: deciding whether to reuse a session id, probing
//! whether the resumed pane survived, and falling back to a fresh launch.

use super::*;

/// Governs whether `start_with_resume_fallback` may pass `--resume <sid>` at
/// all, independent of the per-sid loop-breaker (`resume_probe_failed_sid`),
/// which always applies regardless of policy. `HonorAutoResumeSetting` is
/// used by explicit user restart/reattach (`e`, `Enter`); `Allow` is used by
/// Send Message and Live Send, which must keep trying to preserve agent
/// context even when the user has disabled auto-resume for manual restarts.
/// See #2609.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeAttemptPolicy {
    HonorAutoResumeSetting,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    Alive,
    Dead,
}

const RESUME_PROBE_MAX: std::time::Duration = std::time::Duration::from_millis(3000);

const RESUME_PROBE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Grace window we keep observing after the pane stops running its boot
/// shell, before declaring `Alive`. Sized to cover the longest in-pane
/// boot a real agent takes before it would have crashed on a bad sid:
/// opencode (bun-compiled native binary that loads JS, parses argv, and
/// hits the session-not-found path) reaches `pane_dead = true` between
/// ~900ms and ~1100ms after spawn on a warm cache, longer on cold or
/// heavy projects. Healthy resumes pay this entire window once; the pane is
/// fully attachable for the duration so the cost is purely in the synchronous
/// restart path's latency, not in agent responsiveness afterward.
const RESUME_PROBE_POST_SHELL_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Pure decision: should a launch with this sid/tool use the resume probe?
/// Extracted for unit-testability: the probe path itself needs a real tmux
/// session to test end-to-end.
pub(crate) fn should_attempt_resume(agent_session_id: Option<&str>, tool: &str) -> bool {
    let valid = agent_session_id.map(is_valid_session_id).unwrap_or(false);
    if !valid {
        return false;
    }
    !matches!(
        crate::agents::get_agent(tool).map(|a| &a.resume_strategy),
        Some(crate::agents::ResumeStrategy::Unsupported) | None,
    )
}

impl Instance {
    pub fn restart_with_size(&mut self, size: Option<(u16, u16)>) -> Result<StartOutcome> {
        self.restart_with_size_opts(size, false)
    }

    /// Restart the session, optionally skipping on_launch hooks (e.g. when they
    /// already ran in the background creation poller).
    pub fn restart_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<StartOutcome> {
        self.restart_with_resume_policy(
            size,
            skip_on_launch,
            ResumeAttemptPolicy::HonorAutoResumeSetting,
        )
    }

    pub(crate) fn restart_with_resume_policy(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(size, skip_on_launch, resume_policy, true)
    }

    /// Settle-based pane probe used by the resume-fallback cascade.
    ///
    /// Returns `Dead` immediately if the pane dies or the session evaporates
    /// during the probe window. Returns `Alive` only after the pane has been
    /// off the boot shell for `RESUME_PROBE_POST_SHELL_GRACE` consecutive
    /// time (handles agents whose boot wrapper sits before the agent
    /// crashes on a bad sid), or charitably on full timeout for slow-start
    /// agents. `pane_dead` is the unambiguous signal we trust to fire the
    /// cascade.
    ///
    /// For instances using a shell-wrapper command (`/bin/sh -c '...'`,
    /// agent-override scripts), `is_pane_running_shell` stays true for the
    /// entire probe and the post-shell grace shortcut never fires. Such
    /// instances rely exclusively on `pane_dead`: if the wrapper exits
    /// when the agent crashes, the cascade fires correctly; if the wrapper
    /// holds the pane open past the agent crash (e.g., trailing `sleep`),
    /// the cascade misses it. Pathological shape; not worth special-casing.
    ///
    /// Latency consequence: shell-wrapper instances therefore burn the full
    /// `RESUME_PROBE_MAX` on every healthy resume. Real agents settle in
    /// ~`RESUME_PROBE_POST_SHELL_GRACE`.
    fn probe_settle(
        &self,
        max: std::time::Duration,
        poll: std::time::Duration,
    ) -> Result<ProbeResult> {
        let session = self.tmux_session()?;
        let deadline = std::time::Instant::now() + max;
        let mut first_post_shell: Option<std::time::Instant> = None;
        loop {
            if !session.exists() {
                return Ok(ProbeResult::Dead);
            }
            if session.is_pane_dead() {
                return Ok(ProbeResult::Dead);
            }
            let now = std::time::Instant::now();
            if !session.is_pane_running_shell() {
                let started = *first_post_shell.get_or_insert(now);
                if now.duration_since(started) >= RESUME_PROBE_POST_SHELL_GRACE {
                    return Ok(ProbeResult::Alive);
                }
            } else {
                first_post_shell = None;
            }
            if now >= deadline {
                return Ok(ProbeResult::Alive);
            }
            std::thread::sleep(poll);
        }
    }

    /// Start the session with a one-shot resume fallback.
    ///
    /// Cascade:
    ///   1. If a valid `agent_session_id` is set and the agent supports
    ///      resume, attempt the start (which appends `--resume <sid>` or
    ///      equivalent). Probe the pane via `probe_settle`.
    ///   2. If the pane went dead within the probe window, stop the poller,
    ///      tear down the dead tmux session, preserve the sid, persist a
    ///      `resume_probe_failed_sid` loop-breaker, and return
    ///      `StartOutcome::ResumeFailed`. A dead pane is not proof that the
    ///      sid is invalid, so this path must not clear it or launch fresh.
    ///   3. A launch that pins an already-stored id without resuming it
    ///      (`--session-id <sid>`, or a fork's pre-generated child id) is
    ///      probed the same way, but a death there fails the call outright
    ///      rather than arming the resume loop-breaker: nothing was resumed,
    ///      so there is no resume to break. See `probe_pinned_fresh_launch`.
    ///
    /// `resume_policy` gates step 1: `HonorAutoResumeSetting` additionally
    /// requires `SessionConfig::auto_resume_on_restart`; `Allow` always
    /// permits an attempt (subject to `should_attempt_resume`). Independent
    /// of policy, a sid that already equals `resume_probe_failed_sid` from a
    /// prior call never re-attempts resume: it returns
    /// `StartOutcome::FreshAfterFailedResume` instead of repeating the same
    /// doomed probe. See #2609.
    ///
    /// Latency: only fires the probe when a freshly-created tmux session is
    /// being handed an id AoE already had stored (step 1 or step 3). Healthy
    /// launches on real agents pay `RESUME_PROBE_POST_SHELL_GRACE` (~2s) once
    /// on cold start; warm sessions and brand-new ones pay nothing.
    /// Shell-wrapper command overrides pay the full `RESUME_PROBE_MAX` (~3s) on
    /// every healthy resume because `is_pane_running_shell` never clears for
    /// them; see `probe_settle`. When the failure path fires, add
    /// `kill_clean` (~100ms macOS grace) before returning.
    ///
    /// Acp-mode sessions short-circuit (no tmux pane to probe).
    /// `StartOutcome::Fresh` is honest there: structured view's resume concept lives
    /// in `acp_session_id` and is handled by the ACP supervisor, not
    /// by this cascade.
    pub(crate) fn start_with_resume_fallback(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(size, skip_on_launch, resume_policy, false)
    }

    fn orchestrate_resume_launch(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
        restart: bool,
    ) -> Result<StartOutcome> {
        crate::session::validate_instance_id(&self.id)
            .context("refusing to start: AOE_INSTANCE_ID failed validation")?;
        if self.is_structured() {
            return Ok(StartOutcome::Fresh);
        }
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;

        let title_lock = crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to acquire instance start title lock")?;
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance start lock")?;
        self.reconcile_from_disk();
        if self.is_structured() {
            return Ok(StartOutcome::Fresh);
        }
        if !restart && self.tmux_session()?.exists() {
            return Ok(StartOutcome::Fresh);
        }
        if self.status == Status::Error {
            self.status = Status::Idle;
            self.last_error = None;
            self.last_error_check = None;
        }
        self.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )?;
        if restart {
            self.stop_and_flush_poller_lifecycle_locked();
            self.capture_omp_before_restart(&profile);
        }

        // Keep the generation reservation durable, but allow hooks to invoke
        // aoe against this session without waiting on either flock. Reacquire
        // title before lifecycle and reload (`reconcile_from_disk`) before
        // deriving the launch name: `spawn_prepared_launch`'s `tmux_session()`
        // reads `self.title`, so the reload guarantees the tmux name comes
        // from the authoritative committed title, not a pre-hook value.
        drop(lifecycle_lock);
        drop(title_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, &profile);
        let (_title_lock, _lifecycle_lock) =
            self.reacquire_launch_locks_after_hooks(&storage, hook_result)?;
        let skipped_failed_resume_sid = self.apply_resume_policy(resume_policy);
        self.apply_fresh_launch_intent();

        let prepared = match self.prepare_launch_command() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail_reserved_launch(&storage, &error, false);
                return Err(error);
            }
        };
        let result = (|| {
            if restart {
                self.kill_clean_locked()?;
            }
            let launch_outcome = self.spawn_prepared_launch(size, &profile, prepared)?;
            let outcome =
                self.finish_resume_launch(launch_outcome, skipped_failed_resume_sid, &profile)?;
            self.commit_lifecycle_launch(&storage, restart)?;
            Ok(outcome)
        })();
        if let Err(error) = result {
            self.fail_reserved_launch(&storage, &error, true);
            return Err(error);
        }
        result
    }

    fn apply_resume_policy(&mut self, resume_policy: ResumeAttemptPolicy) -> Option<String> {
        if self.resume_intent != ResumeIntent::Default {
            return None;
        }
        let sid = self.agent_session_id.clone()?;
        let resume_allowed_by_policy = match resume_policy {
            ResumeAttemptPolicy::Allow => true,
            ResumeAttemptPolicy::HonorAutoResumeSetting => {
                crate::session::profile_config::resolve_config_or_warn(&self.effective_profile())
                    .session
                    .auto_resume_on_restart
            }
        };
        if !should_attempt_resume(Some(&sid), &self.tool) {
            return None;
        }
        if self.resume_probe_failed_sid.as_deref() == Some(&sid) {
            self.force_fresh_next_launch = true;
            return Some(sid);
        }
        if !resume_allowed_by_policy {
            self.force_fresh_next_launch = true;
        }
        None
    }

    /// Fail the launch when a fresh-but-pinned start (`--session-id <sid>` on
    /// an id the session already had stored) died inside the probe window.
    ///
    /// The agent rejects a `--session-id` it considers live ("Session ID ... is
    /// already in use") and exits at once. `remain-on-exit` then holds the dead
    /// pane, so the tmux name stays claimed and every later start sees an
    /// existing session and no-ops without saying why. Returning `Err` routes
    /// through `fail_reserved_launch`, which tears the corpse down and records
    /// the pane's own message on the session. See #3399.
    ///
    /// Latency: the same window a resume attempt pays (~2s to settle, 3s max),
    /// on the two launch shapes that can carry a doomed id. A brand-new
    /// session never reaches here.
    fn probe_pinned_fresh_launch(&mut self, sid: &str) -> Result<()> {
        let probe = self.probe_settle(RESUME_PROBE_MAX, RESUME_PROBE_POLL);
        if matches!(probe, Ok(ProbeResult::Alive)) {
            return Ok(());
        }
        self.stop_poller();
        self.session_id_poller = None;
        probe?;
        let detail = self.dead_pane_detail();
        anyhow::bail!("agent exited immediately when pinned to session id {sid}{detail}")
    }

    /// Last line of the agent's own output in the dead pane, as a ": <line>"
    /// suffix for an error message. `remain-on-exit` keeps the content
    /// readable, so this surfaces the agent's diagnosis ("Session ID ... is
    /// already in use") rather than a generic failure. tmux appends its own
    /// `Pane is dead (status N)` banner below that output; skip it, since the
    /// caller already knows the pane died.
    ///
    /// `capture_pane` captures with `-e`, so the agent's line still carries the
    /// SGR sequences it was printed with (an agent error line is routinely red).
    /// Strip them before this lands in `last_error`, which is persisted and
    /// rendered as plain text by the TUI and the dashboard; stripping first also
    /// keeps the banner filter working when `remain-on-exit-format` is styled.
    fn dead_pane_detail(&self) -> String {
        self.tmux_session()
            .ok()
            .and_then(|session| session.capture_pane(20).ok())
            .and_then(|output| {
                crate::tmux::utils::strip_ansi(&output)
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with("Pane is dead"))
                    .map(|line| format!(": {line}"))
            })
            .unwrap_or_default()
    }

    fn finish_resume_launch(
        &mut self,
        launch_outcome: LaunchSidOutcome,
        skipped_failed_resume_sid: Option<String>,
        profile: &str,
    ) -> Result<StartOutcome> {
        let (attempted_sid, pinned_prior_sid) = match launch_outcome {
            LaunchSidOutcome::Existing { sid } if should_attempt_resume(Some(&sid), &self.tool) => {
                (Some(sid), None)
            }
            LaunchSidOutcome::Fresh { pinned_prior_sid }
                if should_attempt_resume(pinned_prior_sid.as_deref(), &self.tool) =>
            {
                (None, pinned_prior_sid)
            }
            _ => (None, None),
        };
        let Some(stale_sid) = attempted_sid else {
            if let Some(sid) = pinned_prior_sid {
                self.probe_pinned_fresh_launch(&sid)?;
            }
            return Ok(match skipped_failed_resume_sid {
                Some(sid) => StartOutcome::FreshAfterFailedResume { sid },
                None => StartOutcome::Fresh,
            });
        };

        let probe = match self.probe_settle(RESUME_PROBE_MAX, RESUME_PROBE_POLL) {
            Ok(probe) => probe,
            Err(error) => {
                self.stop_poller();
                self.session_id_poller = None;
                return Err(error);
            }
        };
        if probe == ProbeResult::Alive {
            return Ok(StartOutcome::Resumed);
        }

        tracing::warn!(
            target: "session.store",
            "start: resume with sid {} for session {} crashed pane within probe; \
             preserving sid and marking resume failure",
            stale_sid,
            self.id,
        );
        self.stop_poller();
        self.session_id_poller = None;
        self.resume_probe_failed_sid = Some(stale_sid.clone());
        if self.mark_resume_probe_failed(profile, &stale_sid) == SidWrite::Failed {
            anyhow::bail!(
                "resume probe failed for sid {} for {}, but marker could not be persisted",
                stale_sid,
                self.id,
            );
        }
        self.kill_clean_locked()
            .with_context(|| format!("kill_clean before resume fallback for {}", self.id))?;
        self.status = Status::Error;
        self.last_error = Some(format!(
            "resume failed for sid {}; preserved for explicit retry",
            stale_sid
        ));
        self.last_error_check = Some(std::time::Instant::now());
        Ok(StartOutcome::ResumeFailed { sid: stale_sid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::instance::launch_command::build_resume_flags;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    fn no_sid_does_not_attempt_resume() {
        assert!(!should_attempt_resume(None, "claude"));
        assert!(!should_attempt_resume(Some(""), "claude"));
        assert!(!should_attempt_resume(Some("   "), "claude"));
    }

    #[test]
    fn invalid_sid_does_not_attempt_resume() {
        assert!(!should_attempt_resume(Some("bad id!"), "claude"));
        assert!(!should_attempt_resume(Some("path/slash"), "claude"));
        assert!(!should_attempt_resume(Some(&"x".repeat(257)), "claude"));
    }

    #[test]
    fn valid_sid_for_resume_supporting_agent_attempts() {
        assert!(should_attempt_resume(
            Some("11111111-1111-1111-1111-111111111111"),
            "claude"
        ));
        assert!(should_attempt_resume(Some("session_abc.123"), "opencode"));
        assert!(should_attempt_resume(Some("uuid-abc-123"), "codex"));
        assert!(should_attempt_resume(Some("uuid-abc-123"), "gemini"));
        assert!(should_attempt_resume(Some("uuid-abc-123"), "copilot"));
    }

    #[test]
    fn resume_capability_controls_launch_poller_and_recovery() {
        let sid = "11111111-1111-1111-1111-111111111111";
        let cases = [
            ("cursor", false),
            ("qwen", false),
            ("kiro", false),
            ("claude", true),
            ("opencode", true),
        ];

        for (tool, supported) in cases {
            let mut inst = Instance::new("resume-contract", "/tmp/test");
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());

            assert_eq!(
                should_attempt_resume(Some(sid), tool),
                supported,
                "{tool}: resume-probe decision"
            );
            assert_eq!(
                inst.supports_session_poller(),
                supported,
                "{tool}: poller capability"
            );
            assert_eq!(
                crate::session::recovery::is_recovery_candidate(&inst),
                supported,
                "{tool}: startup recovery eligibility"
            );

            let mut command = crate::agents::get_agent(tool)
                .unwrap()
                .launch_base_command();
            let base_command = command.clone();
            let resumed = inst.apply_session_flags(&mut command, "test");
            assert_eq!(resumed, supported, "{tool}: launch resume decision");
            assert_eq!(
                command != base_command,
                supported,
                "{tool}: resume argv emission: {command}"
            );
            assert_eq!(
                build_resume_flags(tool, sid, true).is_empty(),
                !supported,
                "{tool}: direct resume flags"
            );
            if matches!(tool, "qwen" | "kiro") {
                assert_eq!(
                    inst.finish_resume_launch(
                        LaunchSidOutcome::Fresh {
                            pinned_prior_sid: Some(sid.to_string()),
                        },
                        None,
                        "test",
                    )
                    .unwrap(),
                    StartOutcome::Fresh,
                    "{tool}: inert stored ID must not trigger the pinned launch probe"
                );
            }
        }
    }

    #[test]
    fn unknown_tool_does_not_attempt_resume() {
        assert!(!should_attempt_resume(Some("uuid-abc-123"), "nonexistent"));
    }

    #[test]
    fn launch_sid_outcome_carries_emitted_sid() {
        let outcome = LaunchSidOutcome::Existing {
            sid: "11111111-1111-1111-1111-111111111111".to_string(),
        };

        match outcome {
            LaunchSidOutcome::Existing { sid } => {
                assert_eq!(sid, "11111111-1111-1111-1111-111111111111");
            }
            other => panic!("expected Existing, got {other:?}"),
        }
    }

    /// This file's own source from `start_marker` up to the tests module.
    /// The end is searched from `start_marker` onward so the slice stays
    /// valid if a `#[cfg(test)]` item is ever added above `mod tests`.
    fn source_from(start_marker: &str) -> String {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session/instance/resume.rs"),
        )
        .unwrap();
        let start = source
            .find(start_marker)
            .unwrap_or_else(|| panic!("start marker not found: {start_marker}"));
        let end = source[start..]
            .find("\n#[cfg(test)]")
            .map_or(source.len(), |offset| start + offset);
        source[start..end].to_string()
    }

    #[test]
    fn start_with_resume_fallback_uses_launch_sid_for_probe_decision() {
        let fallback_source = source_from("pub(crate) fn start_with_resume_fallback");

        assert!(fallback_source
            .contains("let (attempted_sid, pinned_prior_sid) = match launch_outcome"));
        assert!(fallback_source.contains("LaunchSidOutcome::Existing { sid }"));
        assert!(!fallback_source.contains("should_attempt_resume(self.agent_session_id.as_deref()"));
        assert!(!fallback_source.contains("let stale_sid = self\n            .agent_session_id"));
    }

    #[test]
    fn resume_probe_failure_marks_before_cleanup() {
        let fallback_source = source_from("fn finish_resume_launch");
        let local_marker = fallback_source
            .find("self.resume_probe_failed_sid = Some(stale_sid.clone())")
            .unwrap();
        let persisted_marker = fallback_source
            .find("self.mark_resume_probe_failed(profile, &stale_sid)")
            .unwrap();
        let cleanup = fallback_source.find("self.kill_clean_locked()").unwrap();

        assert!(local_marker < cleanup);
        assert!(persisted_marker < cleanup);
    }

    /// Seed a Claude transcript on disk for `sid` under `project_path`, in
    /// the exact location `acquire_session_id`'s existence check reads
    /// (`CLAUDE_CONFIG_DIR` or `$HOME/.claude`). The probe tests below drive
    /// the `--resume` cascade, which acquire now only takes when a stored
    /// sid has a real prior conversation on disk; an empty thread's sid
    /// launches fresh-pinned (`--session-id`) instead. Callers must have set
    /// `HOME` to a temp dir first.
    fn seed_claude_transcript(project_path: &str, sid: &str) {
        let home = std::env::var("CLAUDE_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().expect("home dir").join(".claude"));
        let canonical = std::fs::canonicalize(project_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(project_path));
        let dir = home
            .join("projects")
            .join(crate::session::capture::encode_claude_project_path(
                &canonical.to_string_lossy(),
            ));
        std::fs::create_dir_all(&dir).expect("create claude project dir");
        std::fs::write(dir.join(format!("{sid}.jsonl")), "seed\n").expect("write transcript");
    }

    #[test]
    #[serial]
    fn restart_outcome_for_acp_session_is_fresh() {
        let temp = tempdir().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let mut inst = Instance::new("acp_test", "/tmp/x");
        inst.view = crate::session::instance::View::Structured;
        inst.agent_session_id = Some("11111111-1111-1111-1111-111111111111".to_string());
        inst.tool = "claude".to_string();

        let outcome = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(outcome, StartOutcome::Fresh);
    }

    #[test]
    #[serial]
    fn fallback_marks_resume_failed_and_preserves_sid_when_pane_dies() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("fb-test").unwrap();

        let stale_sid = "11111111-1111-1111-1111-111111111111".to_string();
        let mut inst = Instance::new("fallback_dies_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-test".to_string();
        inst.command = "/bin/false".to_string();
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Real prior conversation on disk so acquire takes the --resume path.
        seed_claude_transcript(&inst.project_path, &stale_sid);
        let id = inst.id.clone();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        assert_eq!(
            outcome.unwrap(),
            StartOutcome::ResumeFailed {
                sid: stale_sid.clone(),
            }
        );
        assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(
            inst.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );
        assert_eq!(inst.status, Status::Error);
        assert_eq!(
            inst.last_error.as_deref(),
            Some(
                format!("resume failed for sid {stale_sid}; preserved for explicit retry").as_str()
            )
        );
        assert!(inst.last_error_check.is_some());
        let loaded = storage.load().unwrap();
        let row = loaded.iter().find(|i| i.id == id).expect("instance");
        assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(
            row.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );
    }

    #[test]
    #[serial]
    fn fallback_does_not_launch_fresh_when_command_would_live_without_stale_sid() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("fb-test-live").unwrap();

        let stale_sid = "22222222-2222-2222-2222-222222222222".to_string();
        let mut inst = Instance::new("fallback_lives_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-test-live".to_string();
        inst.command = format!(
            "/bin/sh -c 'case \"$*\" in *{stale}*) exit 1 ;; esac; exec sleep 30' --",
            stale = stale_sid,
        );
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Real prior conversation on disk so acquire takes the --resume path.
        seed_claude_transcript(&inst.project_path, &stale_sid);

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        assert_eq!(
            outcome.unwrap(),
            StartOutcome::ResumeFailed {
                sid: stale_sid.clone(),
            }
        );
        assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(
            inst.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );
        let loaded = storage.load().unwrap();
        let row = loaded.iter().find(|i| i.id == inst.id).expect("instance");
        assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(
            row.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );
    }

    // #2609: `auto_resume_on_restart = false` must stop `--resume <sid>`
    // from ever reaching the launched command on the restart/reattach
    // path (`HonorAutoResumeSetting`), while leaving Send Message / Live
    // Send (`Allow`) unaffected.
    #[test]
    #[serial]
    fn auto_resume_on_restart_false_skips_stored_sid_and_launches_fresh() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        crate::session::config::update_config(|cfg| {
            cfg.session.auto_resume_on_restart = false;
        })
        .unwrap();

        let storage = crate::session::storage::Storage::new_unwatched("fb-toggle-off").unwrap();

        let stale_sid = "44444444-4444-4444-4444-444444444444".to_string();
        let mut inst = Instance::new("fallback_toggle_off_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-toggle-off".to_string();
        // Would die if (and only if) `--resume <stale_sid>` reached the
        // command; with the toggle off it must never be passed, so this
        // process lives.
        inst.command = format!(
            "/bin/sh -c 'case \"$*\" in *{stale}*) exit 1 ;; esac; exec sleep 30' --",
            stale = stale_sid,
        );
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        let outcome = inst.start_with_resume_fallback(
            None,
            true,
            ResumeAttemptPolicy::HonorAutoResumeSetting,
        );

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        assert_eq!(outcome.unwrap(), StartOutcome::Fresh);
        assert_ne!(
            inst.agent_session_id.as_deref(),
            Some(stale_sid.as_str()),
            "toggle off must generate a fresh sid, not reuse the stale one"
        );
    }

    // #2609: Send Message / Live Send (`Allow`) must keep attempting resume
    // regardless of `auto_resume_on_restart`, so a dead pane still surfaces
    // `ResumeFailed` (proving `--resume <sid>` was passed) rather than
    // silently starting fresh and losing agent context.
    #[test]
    #[serial]
    fn allow_policy_still_attempts_resume_when_auto_resume_on_restart_is_false() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        crate::session::config::update_config(|cfg| {
            cfg.session.auto_resume_on_restart = false;
        })
        .unwrap();

        let storage = crate::session::storage::Storage::new_unwatched("fb-allow-ignores").unwrap();

        let stale_sid = "55555555-5555-5555-5555-555555555555".to_string();
        let mut inst = Instance::new("fallback_allow_ignores_toggle_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-allow-ignores".to_string();
        inst.command = "/bin/false".to_string();
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Real prior conversation on disk so acquire takes the --resume path.
        seed_claude_transcript(&inst.project_path, &stale_sid);

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        assert_eq!(
            outcome.unwrap(),
            StartOutcome::ResumeFailed {
                sid: stale_sid.clone(),
            },
            "Allow must ignore auto_resume_on_restart=false and still attempt resume"
        );
    }

    // #2609 core bug: a sid whose resume probe already failed once must
    // never be retried automatically. Reproduces the reported infinite
    // loop (two consecutive `e`/`Enter` presses against the same doomed
    // sid) and proves the second attempt terminates it instead of
    // repeating `ResumeFailed` forever.
    #[test]
    #[serial]
    fn stale_probe_failed_sid_is_not_retried_on_next_attempt() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("fb-loop-break").unwrap();

        let stale_sid = "66666666-6666-6666-6666-666666666666".to_string();
        let mut inst = Instance::new("fallback_loop_break_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-loop-break".to_string();
        inst.command = "/bin/false".to_string();
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Real prior conversation on disk so the FIRST attempt takes the
        // --resume path (and fails); the loop-breaker on the second attempt
        // then fires from the persisted marker, independent of the transcript.
        seed_claude_transcript(&inst.project_path, &stale_sid);

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        // First attempt: reproduces the pre-existing `ResumeFailed` path,
        // exactly like `fallback_marks_resume_failed_and_preserves_sid_when_pane_dies`.
        let first = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(
            first,
            StartOutcome::ResumeFailed {
                sid: stale_sid.clone(),
            }
        );
        assert_eq!(
            inst.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );

        // Second attempt, same sid, same doomed command: on the pre-fix
        // tree this reproduces the reported bug (identical `ResumeFailed`
        // forever). The fix must instead skip the resume attempt and
        // start fresh.
        inst.kill_clean().unwrap();
        let second = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        assert_eq!(
            second,
            StartOutcome::FreshAfterFailedResume {
                sid: stale_sid.clone(),
            },
            "a sid that already failed a resume probe must not be retried automatically"
        );
        assert_ne!(
            inst.agent_session_id.as_deref(),
            Some(stale_sid.as_str()),
            "loop-breaker must generate a fresh sid instead of repeating the doomed one"
        );
        assert_eq!(
            inst.resume_probe_failed_sid, None,
            "loop-breaker's fresh launch clears the stale marker, matching ResumeIntent::Cleared semantics"
        );
    }

    #[test]
    #[serial]
    fn resume_failed_fires_when_pane_dies_inside_post_shell_grace_window() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));

        let storage = crate::session::storage::Storage::new_unwatched("fb-test-grace").unwrap();

        let stale_sid = "33333333-3333-3333-3333-333333333333".to_string();
        let mut inst = Instance::new("fallback_grace_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-test-grace".to_string();
        inst.command = format!(
            "/bin/sh -c 'case \"$*\" in *{stale}*) exec sleep 1.2 ;; esac; exec sleep 30' --",
            stale = stale_sid,
        );
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Real prior conversation on disk so acquire takes the --resume path.
        seed_claude_transcript(&inst.project_path, &stale_sid);

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        match outcome {
            Ok(StartOutcome::ResumeFailed { sid }) => assert_eq!(sid, stale_sid),
            Ok(StartOutcome::Resumed) => panic!(
                "Tier-1 grace shortcut returned Alive before the t=1200ms pane_dead: \
                 RESUME_PROBE_POST_SHELL_GRACE is too short. \
                 Real opencode crashes at ~1000ms; raise the grace constant."
            ),
            Ok(other) => panic!(
                "Expected ResumeFailed or Resumed; got {other:?} (probe path is taking an unexpected branch)"
            ),
            Err(e) => panic!("resume failure should be a typed outcome, got: {e:#}"),
        }
        assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(
            inst.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );
    }
}
