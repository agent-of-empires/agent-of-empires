//! The launch path: reserving, spawning, and finalizing a tmux session.

use super::*;

/// Outcome of `start_with_resume_fallback`.
///
/// Tmux/process failures propagate as `Err` so callers keep the existing
/// `Status::Error` + `last_error` path. Resume-probe death is represented
/// explicitly as `ResumeFailed` because it preserves durable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Session ID was set and resume succeeded; pane is alive.
    Resumed,
    /// Resume was attempted, but the pane died during the probe before AoE
    /// observed an explicit invalid-resume signal. The sid was preserved and
    /// marked so startup recovery does not retry it automatically.
    ResumeFailed { sid: String },
    /// No resume cascade ran. Either no prior sid, the agent doesn't support
    /// resume, the sid was invalid, the session is structured view-mode (no tmux
    /// pane), or the tmux session was already alive when entered (so
    /// `start_with_size_opts` was a no-op and the probe had nothing to
    /// detect). The pane is alive on return; whether a fresh launch
    /// actually occurred this call depends on the caller having killed
    /// any pre-existing pane first.
    Fresh,
    /// A resume was skipped, and the session started fresh instead, because
    /// `sid` already failed a resume probe once before. Retrying the
    /// identical sid would only reproduce the original `ResumeFailed`
    /// forever, so this launch routes through `ResumeIntent::Cleared`
    /// instead (same as a manual `aoe session set-session-id ""`): a fresh
    /// sid is assigned and `sid` is not carried forward. Distinct from
    /// `Fresh` so callers can tell the user their conversation did not
    /// resume, instead of silently starting a blank session; the prior
    /// conversation is still reachable through the agent's own resume/
    /// history picker. See #2609.
    FreshAfterFailedResume { sid: String },
}

/// What `start_with_size_opts` did with the agent's session id this call.
/// `start_with_resume_fallback` matches on `Existing` to gate the Tier-1
/// settle probe; without the gate, fresh Claude launches mislabel as
/// `StartOutcome::Resumed` because `acquire_session_id` always assigns a
/// UUID for Claude. `Fresh` carries its own probe gate for the launches that
/// pin an already-stored id (see `pinned_prior_sid`).
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSidOutcome {
    /// `acquire_session_id` reused a prior sid: `ResumeIntent::Use(sid)`,
    /// observed `agent_session_id`, or retroactive-capture hit. The launch
    /// command embedded the agent's resume flag.
    Existing { sid: String },
    /// `acquire_session_id` returned a fresh sid (Claude UUID generation)
    /// or `None`. No prior conversation continued.
    Fresh {
        /// Set when the fresh launch pinned an id the session already had
        /// stored, rather than a UUID minted for a brand-new conversation:
        /// the #2700 empty-thread downgrade (`--session-id <sid>`) and a fork
        /// (whose child id is pre-generated at creation). Both can die on the
        /// spot, for a live id or an unresolvable parent, so both are worth
        /// probing; a genuinely new session cannot and skips the probe.
        /// See #3399.
        pinned_prior_sid: Option<String>,
    },
    /// `start_with_size_opts` short-circuited before `apply_session_flags`
    /// ran: structured view-mode session, or a pre-existing tmux pane that is
    /// still alive (kill_clean cache race). `agent_session_id` was not mutated
    /// this call. A pre-existing *dead* pane is not skipped; it is torn down
    /// and relaunched (#3399).
    Skipped,
}

impl Instance {
    pub fn start(&mut self) -> Result<()> {
        self.start_with_size(None)
    }

    pub fn start_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_with_size_opts(size, false).map(|_| ())
    }

    /// Start the session, optionally skipping on_launch hooks (e.g. when they
    /// already ran in the background creation poller).
    pub fn start_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<LaunchSidOutcome> {
        crate::session::validate_instance_id(&self.id)
            .context("refusing to launch: AOE_INSTANCE_ID failed validation")?;
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;

        let title_lock = crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to acquire instance launch title lock")?;
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance launch lock")?;
        self.reconcile_from_disk();
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        // A `remain-on-exit` corpse still owns the tmux name, so plain
        // `exists()` reads a crashed agent as a running session and start
        // becomes a silent no-op the caller reports as success. Recreate the
        // pane instead, the way restart already does. See #3399.
        let session = self.tmux_session()?;
        let corpse_pane = if session.exists() {
            if !session.is_pane_dead() {
                return Ok(LaunchSidOutcome::Skipped);
            }
            true
        } else {
            false
        };
        self.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )?;

        // The durable reservation excludes peer launches while user hooks run.
        // Both flocks must be absent because a hook may invoke aoe for this
        // same session. Reacquire in the global order afterward and reload the
        // authoritative title (via `reconcile_from_disk`) before deriving the
        // tmux launch name: `spawn_prepared_launch`'s `tmux_session()` reads
        // `self.title`, so the reload guarantees the name comes from the
        // committed title a concurrent rename may have written during hooks,
        // never the pre-hook value.
        drop(lifecycle_lock);
        drop(title_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, &profile);
        let (_title_lock, _lifecycle_lock) =
            self.reacquire_launch_locks_after_hooks(&storage, hook_result)?;
        self.apply_fresh_launch_intent();

        let prepared = match self.prepare_launch_command() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail_reserved_launch(&storage, &error, false);
                return Err(error);
            }
        };
        let result = (|| {
            if corpse_pane {
                self.kill_clean_locked()?;
            }
            let outcome = self.spawn_prepared_launch(size, &profile, prepared)?;
            self.commit_lifecycle_launch(&storage, false)?;
            Ok(outcome)
        })();
        if let Err(error) = result {
            self.fail_reserved_launch(&storage, &error, true);
            return Err(error);
        }
        result
    }

    pub(super) fn apply_fresh_launch_intent(&mut self) {
        if std::mem::take(&mut self.force_fresh_next_launch) {
            self.resume_intent = ResumeIntent::Cleared;
        }
        self.reconcile_sidecar_into_disk();
    }

    pub(super) fn spawn_prepared_launch(
        &mut self,
        size: Option<(u16, u16)>,
        profile: &str,
        mut prepared: PreparedLaunch,
    ) -> Result<LaunchSidOutcome> {
        let session = self.tmux_session()?;
        if session.exists() {
            anyhow::bail!(
                "session {} gained a tmux pane before its reserved launch",
                self.id
            );
        }
        let launch_sid = if prepared.is_existing {
            Some(
                self.agent_session_id
                    .clone()
                    .expect("existing launch command carries agent_session_id"),
            )
        } else {
            None
        };
        // Read before `finalize_launch`, which may replace `agent_session_id`.
        let pinned_prior_sid = self
            .agent_session_id
            .clone()
            .filter(|sid| prepared.expected_prior_sid.as_deref() == Some(sid.as_str()));

        tracing::debug!(
            target: "session.store",
            sandboxed = self.is_sandboxed(),
            has_command = prepared.command.is_some(),
            "agent launch command prepared"
        );

        if self.capture_agent_name() == Some("claude") {
            let _ = crate::hooks::unlink_session_id_via_guard(&self.id);
        }

        let mut omp_capture_metadata = if let Some(plan) = prepared.omp_capture_plan {
            let launched_at_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("system clock predates UNIX_EPOCH during OMP launch")
                .and_then(|elapsed| {
                    u64::try_from(elapsed.as_millis())
                        .context("OMP launch timestamp does not fit in u64")
                })?;
            Some(OmpCaptureMetadata {
                layout: plan.layout,
                launched_at_ms,
                launch_id: plan.launch_id,
                launch_marker: plan.launch_marker,
                routing_fingerprint: plan.routing_fingerprint,
                container_runtime: plan.container_runtime,
            })
        } else {
            None
        };
        let omp_generation_published = self.publish_omp_launch_generation(
            profile,
            omp_capture_metadata.as_ref(),
            prepared.expected_prior_omp_generation.as_deref(),
        );
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            // The launch preamble (`wrap_omp_launch`) rewrites OMP's breadcrumb
            // and writes the capture marker only if the store's terminal-sessions
            // directory already exists; it otherwise falls through to a raw
            // launch and capture silently no-ops. A first-ever OMP launch (or a
            // freshly routed store) has no such directory yet, so ensure it here
            // for the host store. Sandboxed launches resolve a container-side
            // path the host must not create.
            if !self.is_sandboxed() {
                if let Err(error) = std::fs::create_dir_all(&metadata.layout.terminal_sessions) {
                    tracing::warn!(
                        target: "session.store",
                        instance = %self.id,
                        "OMP capture may no-op: could not ensure terminal-sessions dir: {error}"
                    );
                }
            }
            prepared.launch_env.pane.push(tmux::PaneEnvMutation::set(
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY.to_string(),
                metadata.launch_id.clone(),
            ));
        }
        session.create_with_size_env_and_container_env(
            &self.project_path,
            prepared.command.as_deref(),
            size,
            profile,
            &prepared.launch_env.pane,
            &prepared.launch_env.container,
        )?;
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            let pane_generation =
                crate::tmux::env::get_env(session.name(), crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY);
            if !omp_generation_published
                || pane_generation.as_deref() != Some(metadata.launch_id.as_str())
            {
                omp_capture_metadata = None;
            }
        }

        self.finalize_launch(
            session.name(),
            profile,
            prepared.expected_prior_sid.as_deref(),
            prepared.expected_prior_intent,
            omp_capture_metadata,
        );

        Ok(match launch_sid {
            Some(sid) => LaunchSidOutcome::Existing { sid },
            None => LaunchSidOutcome::Fresh { pinned_prior_sid },
        })
    }

    /// Post-launch setup: persist state, start pollers, and apply tmux options.
    pub(super) fn finalize_launch(
        &mut self,
        session_name: &str,
        profile: &str,
        expected_prior_sid: Option<&str>,
        expected_prior_intent: ResumeIntent,
        mut omp_capture_metadata: Option<OmpCaptureMetadata>,
    ) {
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            let published = serde_json::to_string(metadata).ok().and_then(|encoded| {
                crate::tmux::env::set_hidden_env(
                    session_name,
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                    &encoded,
                )
                .and_then(|()| {
                    crate::tmux::env::set_hidden_env(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
                        &metadata.launch_id,
                    )
                })
                .ok()
            });
            if published.is_none() {
                omp_capture_metadata = None;
            }
        }

        let outcome = self.persist_session_id(profile, expected_prior_sid, expected_prior_intent);

        // Skip outcomes leave AOE_CAPTURED_SESSION_ID untouched: this path
        // runs before any poller publish, so env is empty for fresh sessions.
        let publish_sid = matches!(outcome, SidPersistOutcome::Published);
        let captured_sid: Option<String> = if publish_sid {
            self.agent_session_id.clone()
        } else {
            None
        };

        let mut entries: Vec<(&str, &str, &str)> = vec![(
            session_name,
            crate::tmux::env::AOE_INSTANCE_ID_KEY,
            &self.id,
        )];
        if let Some(sid) = &captured_sid {
            entries.push((
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                sid.as_str(),
            ));
        }
        if let Err(e) = crate::tmux::env::set_hidden_env_batch(&entries) {
            let keys: Vec<&str> = entries.iter().map(|(_, k, _)| *k).collect();
            tracing::warn!(target: "session.store",
                "Failed to set tmux env keys [{}] at finalize_launch: {}", keys.join(", "), e);
        }

        if publish_sid && self.agent_session_id.is_none() {
            if let Err(e) = crate::tmux::env::remove_hidden_env(
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            ) {
                tracing::warn!(target: "session.store",
                    instance = %self.id,
                    "Failed to clear captured sid in tmux env: {}", e);
            }
        }

        self.maybe_start_poller_since(omp_capture_metadata);

        self.status = Status::Starting;
        self.last_start_time = Some(std::time::Instant::now());

        // Apply status bar options in a background thread to avoid blocking
        // the TUI on the multiple tmux subprocess calls they require.
        let session_name = session_name.to_string();
        let instance_id_for_log = self.id.clone();
        let title = self.title.clone();
        let branch = self.worktree_info.as_ref().map(|w| w.branch.clone());
        let sandbox = self.sandbox_display();
        let options_profile = profile.to_string();
        match std::thread::Builder::new()
            .name(format!("finalize-tmux-{}", instance_id_for_log))
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::tmux::status_bar::apply_all_tmux_options(
                        &session_name,
                        &title,
                        branch.as_deref(),
                        sandbox.as_ref(),
                        &options_profile,
                    );
                })) {
                    tracing::error!(target: "session.store", "finalize-tmux thread panicked: {:?}", panic);
                }
            }) {
            Ok(_handle) => {}
            Err(e) => {
                tracing::error!(target: "session.store",
                    session = %instance_id_for_log,
                    error = %e,
                    "Failed to spawn finalize-tmux thread"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_with_size_opts_returns_skipped_for_structured() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        let outcome = inst.start_with_size_opts(None, false).unwrap();
        assert_eq!(outcome, LaunchSidOutcome::Skipped);
    }

    fn instance_with_id(id: &str) -> Instance {
        let mut inst = Instance::new("tampered-id-test", "/tmp");
        inst.id = id.to_string();
        inst
    }

    #[test]
    fn start_with_size_opts_rejects_tampered_instance_id() {
        for poisoned in ["; rm -rf $HOME #", "../etc", ""] {
            let mut instance = instance_with_id(poisoned);
            let result = instance.start_with_size_opts(None, false);
            let err = match result {
                Ok(_) => panic!("must refuse tampered id at launch (id={poisoned:?})"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("AOE_INSTANCE_ID"),
                "error must surface validator failure for id={poisoned:?}, got: {err}"
            );
            assert!(
                !instance.tmux_session().map(|s| s.exists()).unwrap_or(false),
                "no tmux session must exist after refusal for id={poisoned:?}"
            );
        }
    }
}
