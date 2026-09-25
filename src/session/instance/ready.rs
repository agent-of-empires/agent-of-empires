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
    /// Tmux session did not exist and was started via the resume-fallback path.
    Started,
    /// Resume failed ambiguously while trying to start or respawn the pane.
    /// The durable sid remains stored for an explicit retry.
    ResumeFailed { sid: String },
}

/// Errors `ensure_pane_ready` can return.
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
    /// Smart-send precondition: bring this session's tmux pane to a state where
    /// `send_keys_with_delay` is safe.
    pub fn ensure_pane_ready(&mut self) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        self.ensure_pane_ready_with_size(None)
    }

    /// Like [`ensure_pane_ready`](Self::ensure_pane_ready), but seeds a freshly created or
    /// respawned pane at `size` (cols, rows) instead of letting tmux fall back to its 80x24
    /// default.
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

    /// Best-effort wait for a freshly-started pane to settle past its initial shell/splash so
    /// subsequent `send-keys` land in the agent instead of a boot prompt.
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
    fn ensure_pane_ready_refuses_before_reaching_tmux() {
        // (status, structured view, expected refusal)
        let cases = [
            (
                Status::Creating,
                false,
                EnsureReadyError::Transient(Status::Creating),
            ),
            (
                Status::Deleting,
                false,
                EnsureReadyError::Transient(Status::Deleting),
            ),
            (Status::Idle, true, EnsureReadyError::StructuredView),
        ];
        for (status, structured, want) in cases {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.status = status;
            if structured {
                inst.view = View::Structured;
            }
            let error = inst.ensure_pane_ready().unwrap_err();
            assert_eq!(format!("{error:?}"), format!("{want:?}"));
        }
    }

    /// Real-tmux integration: an alive pane yields AlreadyAlive with no
    /// status/start_time mutations. Skipped if tmux isn't installed.
    // Serialized: this test creates and kills a real tmux session.
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
