//! Making a pane ready to receive input before a send.

use super::*;

/// Outcome of `Instance::ensure_pane_ready`. Callers surface this so the user
/// knows what (if anything) happened on their behalf before a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureReadyOutcome {
    /// Pane was already alive; no action taken.
    AlreadyAlive,
    /// Pane was dead (`#{pane_dead}=1`) and was respawned via the restart path.
    Respawned,
    /// Tmux session did not exist and was started via the resume-fallback
    /// path. Healthy resume and fresh launch both use this outcome;
    /// ambiguous probe failures use `ResumeFailed` instead.
    Started,
    /// Resume failed ambiguously while trying to start or respawn the pane.
    /// The durable sid remains stored for an explicit retry.
    ResumeFailed { sid: String },
}

/// Errors `ensure_pane_ready` can return. Separating transient lifecycle
/// states from real tmux failures lets HTTP callers map them to 409 (retry)
/// vs 500 (real failure) instead of lumping everything as a tmux error.
#[derive(Debug)]
pub enum EnsureReadyError {
    /// Instance is mid-lifecycle (Creating/Deleting). Caller should retry.
    Transient(Status),
    /// Instance is structured view-mode (no backing tmux pane); send is not supported.
    StructuredView,
    /// Underlying tmux operation failed.
    Tmux(anyhow::Error),
}

impl std::fmt::Display for EnsureReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnsureReadyError::Transient(status) => {
                write!(
                    f,
                    "Session is mid-lifecycle ({status:?}); cannot send right now"
                )
            }
            EnsureReadyError::StructuredView => write!(
                f,
                "Acp-mode sessions have no tmux pane; send is not supported"
            ),
            EnsureReadyError::Tmux(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EnsureReadyError {}

impl Instance {
    /// Smart-send precondition: bring this session's tmux pane to a state
    /// where `send_keys_with_delay` is safe.
    ///
    /// Without this, a send to a dead pane silently writes keystrokes to a
    /// corpse with no agent to respond, and the user sees no error.
    ///
    /// Handles three states the caller would otherwise hit:
    /// - Tmux session missing: start from scratch via `start_with_size`.
    /// - Pane dead (`#{pane_dead}=1`): reuse the restart path (same path
    ///   E/F5 uses; well-tested).
    /// - Already alive: no-op.
    ///
    /// Bails on Creating/Deleting (transient lifecycle states) and on
    /// structured view-mode sessions (no backing tmux pane).
    ///
    /// On `Started` / `Respawned`, polls briefly so keystrokes don't race the
    /// agent's startup splash. Best-effort: returns after the timeout even if
    /// the pane is still settling.
    ///
    /// Latency: `AlreadyAlive` is ~tmux RTT. The `Respawned` path routes
    /// through `restart_with_size` -> `start_with_resume_fallback`, which
    /// on a dead resume-eligible pane can block for the resume probe window
    /// (~3s; see `start_with_resume_fallback` for the breakdown) plus up to
    /// 3s of `wait_for_pane_ready` polling.
    /// Smart-send, TUI Enter, and `aoe send` callers should size timeouts
    /// and spinner copy accordingly.
    ///
    /// Note: callers that mutate a clone (e.g. inside `spawn_blocking`) must
    /// sync the post-start state (`status`, `agent_session_id`,
    /// `last_start_time`, `last_error`) back onto the in-memory entry, since
    /// `finalize_launch` writes those fields and they would otherwise be
    /// dropped with the clone. See `apply_post_restart_sync`.
    pub fn ensure_pane_ready(&mut self) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        self.ensure_pane_ready_with_size(None)
    }

    /// Like [`ensure_pane_ready`](Self::ensure_pane_ready), but seeds a
    /// freshly created or respawned pane at `size` (cols, rows) instead of
    /// letting tmux fall back to its 80x24 default.
    ///
    /// Live-send entry passes the visible preview-pane size here so the agent
    /// boots at the width it will be shown at. Without it the agent boots
    /// narrow (80 cols) and depends on a single post-boot `resize-window`
    /// SIGWINCH to grow into the live area. That SIGWINCH races the agent's
    /// startup: if it lands before the agent installs its resize handler the
    /// reflow is lost, and because the per-frame resize loop is deduped on the
    /// (already-correct) tmux window size, nothing re-issues it. The pane then
    /// stays pinned at ~80 cols (≈50% of a wide live area) until live mode is
    /// exited and re-entered. Booting at the right size sidesteps the race.
    ///
    /// `None` keeps tmux's default for callers with no target geometry.
    pub fn ensure_pane_ready_with_size(
        &mut self,
        size: Option<(u16, u16)>,
    ) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        if matches!(self.status, Status::Creating | Status::Deleting) {
            return Err(EnsureReadyError::Transient(self.status));
        }
        if self.is_structured() {
            return Err(EnsureReadyError::StructuredView);
        }
        let session = self.tmux_session().map_err(EnsureReadyError::Tmux)?;
        if !session.exists() {
            // Route fresh starts through the resume probe so a sid loaded
            // from disk that crashes the agent on launch is detected and
            // preserved with a loop-breaker instead of being retried
            // automatically. Always `Allow`: Send Message and Live Send must
            // keep trying to preserve agent context regardless of
            // `auto_resume_on_restart`, which only scopes explicit
            // restart/reattach. See #2609.
            let outcome = self
                .start_with_resume_fallback(size, false, ResumeAttemptPolicy::Allow)
                .map_err(EnsureReadyError::Tmux)?;
            match outcome {
                StartOutcome::ResumeFailed { sid } => {
                    return Ok(EnsureReadyOutcome::ResumeFailed { sid });
                }
                StartOutcome::Resumed
                | StartOutcome::Fresh
                | StartOutcome::FreshAfterFailedResume { .. } => {}
            }
            self.wait_for_pane_ready(&session);
            return Ok(EnsureReadyOutcome::Started);
        }
        if session.is_pane_dead() {
            let outcome = self
                .restart_with_resume_policy(size, false, ResumeAttemptPolicy::Allow)
                .map_err(EnsureReadyError::Tmux)?;
            match outcome {
                StartOutcome::ResumeFailed { sid } => {
                    return Ok(EnsureReadyOutcome::ResumeFailed { sid });
                }
                StartOutcome::Resumed
                | StartOutcome::Fresh
                | StartOutcome::FreshAfterFailedResume { .. } => {}
            }
            self.wait_for_pane_ready(&session);
            return Ok(EnsureReadyOutcome::Respawned);
        }
        Ok(EnsureReadyOutcome::AlreadyAlive)
    }

    /// Best-effort wait for a freshly-started pane to settle past its initial
    /// shell/splash so subsequent `send-keys` land in the agent instead of a
    /// boot prompt. Polls up to 3s in 50ms increments; returns even on
    /// timeout so a sluggish agent doesn't block the send indefinitely.
    ///
    /// Readiness signal:
    /// - Agents that expect a shell, run a custom command override, or have
    ///   an active hook status file: just wait for the pane to not be dead.
    ///   Wrapper scripts look like shells to tmux, so `is_pane_running_shell`
    ///   would never clear for them and we would eat the full 3s every time.
    ///   This mirrors the same guard chain `ensure_session` uses.
    /// - Real agents (e.g. claude, opencode): also wait for the pane to no
    ///   longer be running a shell, so a keystroke doesn't land in the boot
    ///   prompt that runs before the agent binary takes over.
    fn wait_for_pane_ready(&self, session: &tmux::Session) {
        let shell_check_unreliable = self.expects_shell()
            || self.has_command_override()
            || crate::hooks::read_hook_status(&self.id).is_some();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
        loop {
            if !session.exists() {
                return;
            }
            let pane_alive = !session.is_pane_dead();
            if pane_alive && (shell_check_unreliable || !session.is_pane_running_shell()) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_pane_ready_bails_on_creating() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Creating;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::Transient(Status::Creating)) => {}
            other => panic!("expected Transient(Creating), got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_deleting() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Deleting;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::Transient(Status::Deleting)) => {}
            other => panic!("expected Transient(Deleting), got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_structured() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.view = View::Structured;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::StructuredView) => {}
            other => panic!("expected StructuredView, got {other:?}"),
        }
    }

    /// Real-tmux integration: an alive pane yields AlreadyAlive with no
    /// status/start_time mutations. Skipped if tmux isn't installed.
    // Serialized: this test creates and kills a real tmux session. Unserialized
    // it can kill the shared server's last session while a `#[serial]` peer's
    // `new-session` is connecting, which fails that peer with "server exited
    // unexpectedly" (and its own skip-on-failure fallback silently masks the
    // same race in the other direction).
    #[test]
    #[serial_test::serial]
    fn test_ensure_pane_ready_alive_pane_is_noop() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }

        let mut inst = Instance::new("ensure_alive_test", "/tmp/test");
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &tmux_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep",
                "60",
            ])
            .status();
        if !created.map(|s| s.success()).unwrap_or(false) {
            eprintln!("tmux new-session failed; skipping");
            return;
        }
        crate::tmux::refresh_session_cache();

        inst.status = Status::Running;
        let prev_start = inst.last_start_time;
        let prev_status = inst.status;

        let outcome = inst.ensure_pane_ready().expect("ensure_pane_ready ok");
        assert_eq!(outcome, EnsureReadyOutcome::AlreadyAlive);
        assert_eq!(inst.last_start_time, prev_start);
        assert_eq!(inst.status, prev_status);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();
    }
}
