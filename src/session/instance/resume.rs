//! Restart and resume: deciding whether to reuse a session id, probing
//! whether the resumed pane survived, and falling back to a fresh launch.

use super::*;

/// Governs whether `start_with_resume_fallback` may pass `--resume <sid>` at all, independent of
/// the per-sid loop-breaker (`resume_probe_failed_sid`), which always applies regardless of policy.
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

/// Grace window we keep observing after the pane stops running its boot shell, before declaring
/// `Alive`.
const RESUME_PROBE_POST_SHELL_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

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
        self.orchestrate_resume_launch(size, skip_on_launch, resume_policy, true, false, None)
    }

    /// Restart, first removing the sandbox container when `discard_sandbox_container` is set so the
    /// launch recreates it with the current tool's mounts, and carrying the conversation into the
    /// incoming account's config root when the swap changed only the account (#4030). Removal
    /// happens only once this restart owns the Launch reservation.
    pub fn restart_discarding_sandbox_container(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        discard_sandbox_container: bool,
        conversation_carry: Option<ConversationCarry>,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(
            size,
            skip_on_launch,
            ResumeAttemptPolicy::HonorAutoResumeSetting,
            true,
            discard_sandbox_container,
            conversation_carry,
        )
    }

    /// Settle-based pane probe used by the resume-fallback cascade.
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
                #[cfg(test)]
                let entered_post_shell = first_post_shell.is_none();
                let started = *first_post_shell.get_or_insert(now);
                #[cfg(test)]
                if entered_post_shell {
                    tests::post_shell_observed(&session);
                }
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
    pub(crate) fn start_with_resume_fallback(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(size, skip_on_launch, resume_policy, false, false, None)
    }

    fn orchestrate_resume_launch(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
        restart: bool,
        discard_sandbox_container: bool,
        conversation_carry: Option<ConversationCarry>,
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
        if discard_sandbox_container {
            if let Err(error) = self.discard_stale_sandbox_container() {
                self.fail_reserved_launch(&storage, &error, false);
                return Err(error);
            }
        }

        // Keep the generation reservation durable, but allow hooks to invoke aoe against this
        // session without waiting on either flock.
        drop(lifecycle_lock);
        drop(title_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, &profile);
        let (_title_lock, _lifecycle_lock) =
            self.reacquire_launch_locks_after_hooks(&storage, hook_result)?;
        let skipped_failed_resume_sid = self.apply_resume_policy(resume_policy);
        self.apply_fresh_launch_intent();

        let result = (|| {
            let prepared = self.stop_carry_and_prepare(restart, conversation_carry)?;
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

    /// Tear down the outgoing pane, carry the conversation, then build the
    /// launch command, in that order.
    ///
    /// The order is load-bearing at both ends, which is why all three steps
    /// live here rather than spread through the cascade. The outgoing agent
    /// appends to its transcript for as long as it runs, so a carry before the
    /// teardown copies a file that is still growing, and the incoming
    /// account's copy is never repaired once published. And
    /// `build_launch_command` chooses `--resume <sid>` or `--session-id <sid>`
    /// from whether the incoming account's transcript exists, so a carry after
    /// it pins an id the agent then rejects as already in use, killing the pane
    /// (#3399).
    fn stop_carry_and_prepare(
        &mut self,
        restart: bool,
        conversation_carry: Option<ConversationCarry>,
    ) -> Result<PreparedLaunch> {
        if restart {
            self.kill_clean_locked()?;
        }
        if let Some(carry) = conversation_carry {
            carry.run();
        }
        let prepared = self.prepare_launch_command()?;
        if restart {
            return self.refresh_prepared_prime_launch_after_pane_stop(prepared);
        }
        Ok(prepared)
    }

    /// A failure fails the restart: relaunching into the old container would run the new tool
    /// against the previous tool's config store.
    fn discard_stale_sandbox_container(&self) -> Result<()> {
        if !self.is_sandboxed() {
            return Ok(());
        }
        let container = DockerContainer::from_session_id(&self.id);
        match container.discard() {
            crate::containers::Teardown::Removed => {
                tracing::info!(
                    target: "containers.runtime",
                    session = %self.id,
                    "removed sandbox container built for the previous tool; it will be recreated on start"
                );
                Ok(())
            }
            crate::containers::Teardown::AlreadyGone => Ok(()),
            crate::containers::Teardown::Failed(e) => anyhow::bail!(
                "failed to remove sandbox container {} built for the previous tool; remove it \
                 before restarting, or the new tool reuses its config: {e}",
                container.name
            ),
        }
    }

    fn apply_resume_policy(&mut self, resume_policy: ResumeAttemptPolicy) -> Option<String> {
        if self.resume_intent != ResumeIntent::Default {
            return None;
        }
        let sid = self.agent_session_id.clone()?;
        let resume_allowed_by_policy = match resume_policy {
            ResumeAttemptPolicy::Allow => true,
            ResumeAttemptPolicy::HonorAutoResumeSetting => {
                crate::session::config::profile_config::resolve_config_or_warn(
                    &self.effective_profile(),
                )
                .session
                .auto_resume_on_restart
            }
        };
        if !is_valid_session_id(&sid) || !self.supports_native_resume() {
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

    /// Fail the launch when a fresh-but-pinned start (`--session-id <sid>` on an id the session
    /// already had stored) died inside the probe window.
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

    /// Last line of the agent's own output in the dead pane, as a ": <line>" suffix for an error
    /// message.
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

    fn should_probe_pinned_fresh_launch(&self, sid: Option<&str>) -> bool {
        sid.is_some_and(is_valid_session_id) && self.supports_native_resume()
    }

    fn finish_resume_launch(
        &mut self,
        launch_outcome: LaunchSidOutcome,
        skipped_failed_resume_sid: Option<String>,
        profile: &str,
    ) -> Result<StartOutcome> {
        let (attempted_sid, pinned_prior_sid) = match launch_outcome {
            LaunchSidOutcome::Existing { sid }
                if is_valid_session_id(&sid) && self.supports_native_resume() =>
            {
                (Some(sid), None)
            }
            LaunchSidOutcome::Fresh { pinned_prior_sid }
                if self.should_probe_pinned_fresh_launch(pinned_prior_sid.as_deref()) =>
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
    use crate::session::instance::test_helpers::install_aliases;
    use serial_test::serial;
    use tempfile::tempdir;

    /// Pins the order inside `stop_carry_and_prepare`: the launch command is
    /// built from whether the incoming account's transcript exists, so the
    /// carry has to have published it by then. Preparing first yields
    /// `--session-id <sid>` on an id the agent rejects as already in use once
    /// the carry creates the transcript (#3399, #4030).
    #[test]
    #[serial]
    fn carry_runs_before_the_launch_command_picks_its_resume_flag() {
        const SID: &str = "11111111-2222-3333-4444-555555555555";
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let home = dirs::home_dir().expect("home");
        let app_dir = crate::session::get_app_dir().expect("app dir");
        std::fs::create_dir_all(&app_dir).expect("app dir");
        std::fs::write(
            app_dir.join("config.toml"),
            "[session.agent_detect_as]\n\
             claude-1 = \"claude\"\n\
             claude-2 = \"claude\"\n\
             \n\
             [session.agent_config_dir]\n\
             claude-1 = \"~/dot-claude-1\"\n\
             claude-2 = \"~/dot-claude-2\"\n",
        )
        .expect("config");
        let profile = crate::session::config::effective_profile("");
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
        crate::session::config::profile_config::resolve_config_or_warn(&profile);

        let project = home.join("project");
        std::fs::create_dir_all(&project).expect("project");
        let mut inst = Instance::new("t", project.to_str().unwrap());
        inst.tool = "claude-1".to_string();
        inst.detect_as = "claude".to_string();
        // The renamed-wrapper shape these per-account tools take: a bare token
        // the launch shell resolves, which keeps native resume available.
        inst.command = "claude".to_string();
        inst.agent_session_id = Some(SID.to_string());

        // The outgoing account holds the conversation, the incoming one does not.
        let encoded = crate::session::capture::encode_claude_project_path(
            &crate::session::capture::canonicalize_or_raw(project.to_str().unwrap())
                .to_string_lossy(),
        );
        let seeded = home.join("dot-claude-1").join("projects").join(&encoded);
        std::fs::create_dir_all(&seeded).expect("seed dir");
        std::fs::write(seeded.join(format!("{SID}.jsonl")), "conversation\n").expect("seed");

        let carry = match crate::session::conversation_carry::classify(&inst, &profile, "claude-2")
        {
            crate::session::conversation_carry::ToolSwap::KeepConversation(Some(carry)) => carry,
            other => panic!("expected a planned carry, got {other:?}"),
        };
        inst.swap_account("claude-2");

        let prepared = inst
            .stop_carry_and_prepare(false, Some(carry))
            .expect("prepare");

        assert!(
            prepared.is_existing,
            "the carried transcript must be visible when the flag is chosen"
        );
        assert!(
            prepared
                .command
                .as_deref()
                .is_some_and(|command| command.contains(&format!("--resume {SID}"))),
            "expected --resume on the carried conversation, got: {:?}",
            prepared.command
        );
    }
    type PostShellCallback = Box<dyn FnOnce(&crate::tmux::Session)>;
    thread_local! {
        static POST_SHELL_OBSERVER: std::cell::RefCell<Option<PostShellCallback>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn post_shell_observed(session: &crate::tmux::Session) {
        let observer = POST_SHELL_OBSERVER.with(|slot| slot.borrow_mut().take());
        if let Some(observer) = observer {
            observer(session);
        }
    }

    struct PostShellObserver;
    impl Drop for PostShellObserver {
        fn drop(&mut self) {
            POST_SHELL_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    fn isolate_resume_environment(
        root: &std::path::Path,
    ) -> crate::session::test_support::EnvGuard {
        let roots = vec![
            ("HOME", root.to_path_buf()),
            ("XDG_CONFIG_HOME", root.join(".config")),
            ("CLAUDE_CONFIG_DIR", root.join(".claude")),
        ];
        let guard = crate::session::test_support::EnvGuard::set(&roots);
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        guard
    }

    fn install_fake_claude(
        root: &std::path::Path,
        script: &str,
    ) -> crate::session::test_support::EnvGuard {
        crate::session::test_support::install_login_shell_path_command(root, "claude", script)
    }

    #[test]
    fn resume_and_capture_capabilities_control_each_path() {
        let sid = "11111111-1111-1111-1111-111111111111";
        let cases = [
            ("cursor", true, true),
            ("qwen", false, false),
            ("kiro", false, false),
            ("copilot", true, false),
            ("vibe", true, false),
            ("claude", true, true),
            ("opencode", true, false),
        ];

        for (tool, resume_supported, poller_supported) in cases {
            let mut inst = Instance::new("resume-contract", "/tmp/test");
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());

            assert_eq!(
                inst.supports_native_resume(),
                resume_supported,
                "{tool}: resume-probe decision"
            );
            assert_eq!(
                inst.supports_session_poller(),
                poller_supported,
                "{tool}: poller capability"
            );
            assert_eq!(
                crate::session::recovery::is_recovery_candidate(&inst),
                resume_supported,
                "{tool}: startup recovery eligibility"
            );

            let mut command = crate::agents::get_agent(tool)
                .unwrap()
                .launch_base_command();
            let base_command = command.clone();
            let resumed = inst.apply_session_flags(&mut command, "test").unwrap();
            assert_eq!(resumed, resume_supported, "{tool}: launch resume decision");
            assert_eq!(
                command != base_command,
                resume_supported,
                "{tool}: resume argv emission: {command}"
            );
            assert_eq!(
                build_resume_flags(tool, sid, true).is_empty(),
                !resume_supported,
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

    fn dead_resume_fixture(inst: &Instance) -> crate::tmux::test_helpers::TmuxTestSession {
        use crate::tmux::test_helpers::{only_pane_id, wait_for_pane_dead, TmuxTestSession};
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let guard = TmuxTestSession::from_name(name.clone());
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "true",
                ";",
                "set-option",
                "-p",
                "-t",
                &name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        wait_for_pane_dead(&only_pane_id(&name));
        crate::tmux::refresh_session_cache();
        guard
    }

    #[test]
    #[serial]
    fn start_with_resume_fallback_uses_launch_sid_for_probe_decision() {
        use crate::session::instance::start::test_support::{FinalizeObserver, FinalizePhase};
        const PROFILE: &str = "launch-sid-probe";
        const LAUNCHED_SID: &str = "11111111-1111-1111-1111-111111111111";
        const PEER_SID: &str = "22222222-2222-2222-2222-222222222222";

        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let argv_path = temp.path().join("argv");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 1\n",
            shell_escape(&argv_path.to_string_lossy())
        );
        let _claude = install_fake_claude(temp.path(), &script);
        let mut inst = Instance::new("launch-sid-probe", project.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.command = "claude".to_string();
        inst.source_profile = PROFILE.to_string();
        inst.agent_session_id = Some(LAUNCHED_SID.to_string());
        seed_claude_transcript(&inst.project_path, LAUNCHED_SID);
        let storage = crate::session::storage::Storage::new_unwatched(PROFILE).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(tmux_name.clone());
        let finalized = std::rc::Rc::new(std::cell::Cell::new(false));
        let finalized_in_observer = finalized.clone();
        let _observer = FinalizeObserver::install(inst.id.clone(), move |instance, phase| {
            match phase {
                FinalizePhase::Before => {
                    let pane = crate::tmux::test_helpers::only_pane_id(&tmux_name);
                    crate::tmux::test_helpers::wait_for_pane_dead(&pane);
                    let argv = std::fs::read_to_string(&argv_path).unwrap();
                    let args: Vec<_> = argv.lines().collect();
                    assert!(
                        args.windows(2)
                            .any(|pair| pair == ["--resume", LAUNCHED_SID]),
                        "actual agent argv: {args:?}"
                    );
                    assert_eq!(instance.agent_session_id.as_deref(), Some(LAUNCHED_SID));
                    let peer_storage =
                        crate::session::storage::Storage::new_unwatched(PROFILE).unwrap();
                    peer_storage
                        .update(|rows, _| {
                            let row = rows.iter_mut().find(|row| row.id == instance.id).unwrap();
                            row.agent_session_id = Some(PEER_SID.to_string());
                            Ok(())
                        })
                        .unwrap();
                }
                FinalizePhase::After => {
                    assert_eq!(instance.agent_session_id.as_deref(), Some(PEER_SID), "real finalize CAS skip must reload peer identity before producing the launch outcome");
                    finalized_in_observer.set(true);
                }
            }
        });
        let outcome = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert!(finalized.get(), "native launch must reach finalization");
        assert_eq!(
            outcome,
            StartOutcome::ResumeFailed {
                sid: LAUNCHED_SID.to_string()
            }
        );
        let rows = storage.load().unwrap();
        let row = rows.iter().find(|row| row.id == inst.id).unwrap();
        assert_eq!(row.agent_session_id.as_deref(), Some(PEER_SID));
        assert_eq!(
            row.resume_probe_failed_sid, None,
            "failure of A must not mark the peer's B as failed"
        );
        assert!(!inst.tmux_session().unwrap().exists());
    }

    #[test]
    #[serial]
    fn resume_probe_failure_marks_before_cleanup() {
        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let mut inst = Instance::new("marker-before-cleanup", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.source_profile = "marker-before-cleanup".to_string();
        let launched_sid = "11111111-1111-1111-1111-111111111111";
        inst.agent_session_id = Some(launched_sid.to_string());
        let storage =
            crate::session::storage::Storage::new_unwatched(&inst.source_profile).unwrap();
        assert!(storage.load().unwrap().is_empty());
        let _pane = dead_resume_fixture(&inst);
        let result = inst.finish_resume_launch(
            LaunchSidOutcome::Existing {
                sid: launched_sid.to_string(),
            },
            None,
            "marker-before-cleanup",
        );
        assert!(
            result.is_err(),
            "the missing durable row must reject the failure marker"
        );
        assert_eq!(inst.resume_probe_failed_sid.as_deref(), Some(launched_sid));
        assert!(
            inst.tmux_session().unwrap().exists(),
            "failed marker persistence must not clean up the pane"
        );
        assert!(inst.tmux_session().unwrap().is_pane_dead());
    }

    /// Seed a Claude transcript on disk for `sid` under `project_path`, in the exact location
    /// `acquire_session_id`'s existence check reads (`CLAUDE_CONFIG_DIR` or `$HOME/.claude`).
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
        let _env = isolate_resume_environment(temp.path());

        let mut inst = Instance::new("acp_test", "/tmp/x");
        inst.view = crate::session::instance::View::Structured;
        inst.agent_session_id = Some("11111111-1111-1111-1111-111111111111".to_string());
        inst.tool = "claude".to_string();

        let outcome = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(outcome, StartOutcome::Fresh);
    }

    /// A claude row with a stored sid, seeded on disk, launching a fake `claude` from `script`.
    struct Fallback {
        inst: Instance,
        storage: crate::session::storage::Storage,
        sid: String,
        tmux_name: String,
        _auto_resume: Option<crate::session::test_support::AutoResumeGuard>,
        _claude: crate::session::test_support::EnvGuard,
        _env: crate::session::test_support::EnvGuard,
        _temp: tempfile::TempDir,
    }

    impl Drop for Fallback {
        fn drop(&mut self) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &self.tmux_name])
                .output();
        }
    }

    /// `None` when tmux is unavailable. `seed_transcript` makes acquire take the `--resume` path.
    fn fallback_fixture(
        profile: &str,
        sid: &str,
        script: &str,
        seed_transcript: bool,
        auto_resume: Option<bool>,
    ) -> Option<Fallback> {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return None;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let env = isolate_resume_environment(temp.path());
        let auto_resume = auto_resume.map(crate::session::test_support::AutoResumeGuard::set);
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new(profile, project_dir.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.source_profile = profile.to_string();
        let claude = install_fake_claude(temp.path(), script);
        inst.command = "claude".to_string();
        inst.agent_session_id = Some(sid.to_string());
        inst.status = Status::Idle;
        if seed_transcript {
            seed_claude_transcript(&inst.project_path, sid);
        }
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
        Some(Fallback {
            inst,
            storage,
            sid: sid.to_string(),
            tmux_name,
            _auto_resume: auto_resume,
            _claude: claude,
            _env: env,
            _temp: temp,
        })
    }

    fn dies_on_sid(sid: &str) -> String {
        format!("#!/bin/sh\ncase \"$*\" in *{sid}*) exit 1 ;; esac\nexec sleep 30\n")
    }

    fn assert_failure_marked(f: &Fallback) {
        let sid = Some(f.sid.as_str());
        assert_eq!(f.inst.agent_session_id.as_deref(), sid);
        assert_eq!(f.inst.resume_probe_failed_sid.as_deref(), sid);
        let loaded = f.storage.load().unwrap();
        let row = loaded.iter().find(|i| i.id == f.inst.id).expect("instance");
        assert_eq!(row.agent_session_id.as_deref(), sid);
        assert_eq!(row.resume_probe_failed_sid.as_deref(), sid);
    }

    #[test]
    #[serial]
    fn fallback_marks_resume_failed_and_preserves_sid() {
        // Whether the pane dies outright or only the resume argv is doomed, no fresh launch.
        for (profile, sid, script) in [
            (
                "fb-test",
                "11111111-1111-1111-1111-111111111111",
                "#!/bin/sh\nexit 1\n".to_string(),
            ),
            (
                "fb-test-live",
                "22222222-2222-2222-2222-222222222222",
                dies_on_sid("22222222-2222-2222-2222-222222222222"),
            ),
        ] {
            let Some(mut f) = fallback_fixture(profile, sid, &script, true, None) else {
                return;
            };
            let outcome = f
                .inst
                .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);
            assert_eq!(
                outcome.unwrap(),
                StartOutcome::ResumeFailed {
                    sid: sid.to_string()
                }
            );
            assert_failure_marked(&f);
            if profile == "fb-test" {
                assert_eq!(f.inst.status, Status::Error);
                assert_eq!(
                    f.inst.last_error.as_deref(),
                    Some(
                        format!("resume failed for sid {sid}; preserved for explicit retry")
                            .as_str()
                    )
                );
                assert!(f.inst.last_error_check.is_some());
            }
        }
    }

    /// `auto_resume_on_restart = false` scopes only restart/reattach; `Allow` still resumes (#2609).
    #[test]
    #[serial]
    fn auto_resume_setting_scopes_only_the_honoring_policy() {
        let sid = "44444444-4444-4444-4444-444444444444";
        let Some(mut f) =
            fallback_fixture("fb-toggle-off", sid, &dies_on_sid(sid), false, Some(false))
        else {
            return;
        };
        let outcome = f.inst.start_with_resume_fallback(
            None,
            true,
            ResumeAttemptPolicy::HonorAutoResumeSetting,
        );
        assert_eq!(outcome.unwrap(), StartOutcome::Fresh);
        assert_ne!(f.inst.agent_session_id.as_deref(), Some(sid));
        drop(f);

        let sid = "55555555-5555-5555-5555-555555555555";
        let Some(mut f) = fallback_fixture(
            "fb-allow-ignores",
            sid,
            "#!/bin/sh\nexit 1\n",
            true,
            Some(false),
        ) else {
            return;
        };
        let outcome = f
            .inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);
        assert_eq!(
            outcome.unwrap(),
            StartOutcome::ResumeFailed {
                sid: sid.to_string()
            }
        );
    }

    /// A sid whose resume probe already failed is never retried automatically (#2609).
    #[test]
    #[serial]
    fn stale_probe_failed_sid_is_not_retried_on_next_attempt() {
        let sid = "66666666-6666-6666-6666-666666666666";
        let Some(mut f) = fallback_fixture("fb-loop-break", sid, "#!/bin/sh\nexit 1\n", true, None)
        else {
            return;
        };
        let first = f
            .inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(
            first,
            StartOutcome::ResumeFailed {
                sid: sid.to_string()
            }
        );
        assert_eq!(f.inst.resume_probe_failed_sid.as_deref(), Some(sid));

        f.inst.kill_clean().unwrap();
        let second = f
            .inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(
            second,
            StartOutcome::FreshAfterFailedResume {
                sid: sid.to_string()
            }
        );
        assert_ne!(f.inst.agent_session_id.as_deref(), Some(sid));
        assert_eq!(f.inst.resume_probe_failed_sid, None);
    }

    #[test]
    #[serial]
    fn resume_failed_fires_when_pane_dies_inside_post_shell_grace_window() {
        let sid = "33333333-3333-3333-3333-333333333333";
        let script =
            format!("#!/bin/sh\ncase \"$*\" in *{sid}*) exec sleep 30 ;; esac\nexec sleep 30\n");
        let Some(mut f) = fallback_fixture("fb-test-grace", sid, &script, true, None) else {
            return;
        };
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(f.tmux_name.clone());
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed_in_probe = observed.clone();
        let probe_name = f.tmux_name.clone();
        POST_SHELL_OBSERVER.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |session| {
                assert!(session.exists());
                assert!(!session.is_pane_dead());
                assert!(!session.is_pane_running_shell());
                observed_in_probe.set(true);
                let pane = crate::tmux::test_helpers::only_pane_id(&probe_name);
                let mut kill = crate::tmux::tmux_command();
                kill.args(["send-keys", "-t", &pane, "C-c"]);
                assert!(kill.output().unwrap().status.success());
                crate::tmux::test_helpers::wait_for_pane_dead(&pane);
            }))
        });
        let _observer = PostShellObserver;
        let outcome = f
            .inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);
        assert!(
            observed.get(),
            "the observer must enter the post-shell grace branch before death"
        );
        assert_eq!(
            outcome.unwrap(),
            StartOutcome::ResumeFailed {
                sid: sid.to_string()
            }
        );
        assert_eq!(f.inst.agent_session_id.as_deref(), Some(sid));
        assert_eq!(f.inst.resume_probe_failed_sid.as_deref(), Some(sid));
    }

    #[test]
    fn pinned_fresh_probe_uses_resolved_alias_capability() {
        const PROFILE: &str = "pinned-probe-alias";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);
        let mut inst = Instance::new("alias", "/tmp/x");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "work-claude".to_string();
        inst.command = "claude".to_string();
        assert!(inst.should_probe_pinned_fresh_launch(Some("11111111-2222-3333-4444-555555555555")));
    }
}
