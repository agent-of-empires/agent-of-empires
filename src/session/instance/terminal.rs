//! Companion terminal panes, on the host and inside a container.

use super::*;

/// Command run inside the sandbox container for the web Container terminal tab.
///
/// Resolves the container user's login shell at spawn time, inside the container,
/// and execs it as a login shell so profile/rc files load (parity with the Host
/// terminal tab, which launches the user's default shell as a login shell).
/// Resolution order: the passwd entry (the authoritative login shell, what
/// `chsh` writes and what `login(1)` reads into `$SHELL`), then the container's
/// `$SHELL`, then bash, sh. Passwd comes first because `docker exec` never goes
/// through `login(1)`, so `$SHELL` is usually unset or a generic image default
/// rather than the user's configured shell. Each candidate is run through
/// `command -v` so an unset, stale, or non-executable value falls through to the
/// next instead of killing the pane.
///
/// The single-quoted body is evaluated by the container's `sh`, not the host
/// shell tmux uses to spawn the session, so the embedded `$()` runs in the
/// container. The host does not propagate its own `$SHELL` into the container,
/// so this reads the container's value, not the host's.
const CONTAINER_TERMINAL_AUTODETECT_CMD: &str = r#"sh -c 'exec "$(command -v "$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f7)" 2>/dev/null || command -v "$SHELL" 2>/dev/null || command -v bash || command -v sh)" -l'"#;

impl Instance {
    pub fn terminal_tmux_session(&self) -> Result<tmux::TerminalSession> {
        self.terminal_tmux_session_indexed(0)
    }

    /// Paired host terminal at `index`. Index 0 is the historical single
    /// terminal (the only one the TUI uses); index >= 1 are the additional
    /// web dashboard terminal tabs (#2437).
    pub fn terminal_tmux_session_indexed(&self, index: u32) -> Result<tmux::TerminalSession> {
        tmux::TerminalSession::new_indexed(&self.id, &self.title, index)
    }

    pub fn has_terminal(&self) -> bool {
        self.terminal_info
            .as_ref()
            .map(|t| t.created)
            .unwrap_or(false)
    }

    pub fn start_terminal(&mut self) -> Result<()> {
        self.start_terminal_with_size(None)
    }

    pub fn start_terminal_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_terminal_with_size_indexed(0, size)
    }

    pub fn start_terminal_with_size_indexed(
        &mut self,
        index: u32,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        let session = self.terminal_tmux_session_indexed(index)?;

        let is_new = !session.exists();
        if is_new {
            session.create_with_size(&self.project_path, None, size, &self.effective_profile())?;
            // Apply all configured tmux options to terminal sessions too
            self.apply_terminal_tmux_options(index);
        }

        // The persisted `terminal_info` cache is the index-0 fast path the TUI
        // reads; additional terminals (index >= 1) are tracked by the web
        // dashboard and queried straight from tmux, like container terminals.
        if index == 0 {
            self.terminal_info = Some(TerminalInfo { created: true });
        }

        Ok(())
    }

    pub fn kill_terminal(&self) -> Result<()> {
        self.kill_terminal_indexed(0)
    }

    pub fn kill_terminal_indexed(&self, index: u32) -> Result<()> {
        let session = self.terminal_tmux_session_indexed(index)?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    /// Kill the paired terminal tmux session if its pane is dead (shell
    /// exited while `remain-on-exit on` kept the session as a tombstone).
    /// Returns true if a kill happened so the caller knows to re-spawn.
    /// A missing session or a live pane both return Ok(false).
    pub fn kill_terminal_if_dead(&self) -> Result<bool> {
        self.kill_terminal_if_dead_indexed(0)
    }

    pub fn kill_terminal_if_dead_indexed(&self, index: u32) -> Result<bool> {
        let session = self.terminal_tmux_session_indexed(index)?;
        if session.exists() && session.is_pane_dead() {
            let _ = session.kill();
            return Ok(true);
        }
        Ok(false)
    }

    pub fn container_terminal_tmux_session(&self) -> Result<tmux::ContainerTerminalSession> {
        self.container_terminal_tmux_session_indexed(0)
    }

    pub fn container_terminal_tmux_session_indexed(
        &self,
        index: u32,
    ) -> Result<tmux::ContainerTerminalSession> {
        tmux::ContainerTerminalSession::new_indexed(&self.id, &self.title, index)
    }

    pub fn has_container_terminal(&self) -> bool {
        self.container_terminal_tmux_session()
            .map(|s| s.exists())
            .unwrap_or(false)
    }

    pub fn start_container_terminal_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_container_terminal_with_size_indexed(0, size)
    }

    pub fn start_container_terminal_with_size_indexed(
        &mut self,
        index: u32,
        size: Option<(u16, u16)>,
    ) -> Result<()> {
        if !self.is_sandboxed() {
            anyhow::bail!("Cannot create container terminal for non-sandboxed session");
        }

        let container = self.get_container_for_instance()?;
        let sandbox = self
            .sandbox_info
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sandbox_info missing for sandboxed session"))?;

        let detect_as = self.effective_detect_as().into_owned();
        let managed_codex_home = container_config::managed_codex_home(
            &self.tool,
            Some(detect_as.as_str()),
            &self.source_profile,
            &self.id,
        )?;
        let env_info = build_docker_env_args_with_managed_codex_home(
            &self.source_profile,
            sandbox,
            std::path::Path::new(&self.project_path),
            managed_codex_home.as_deref(),
        );
        let env_part = if env_info.docker_args.is_empty() {
            String::new()
        } else {
            format!("{} ", env_info.docker_args)
        };

        // Get workspace path inside container (handles bare repo worktrees correctly)
        let container_workdir = self.container_workdir();

        let cmd = container.exec_command(
            Some(&format!("-w {} {}", container_workdir, env_part)),
            CONTAINER_TERMINAL_AUTODETECT_CMD,
        );

        // Values ride the protected env-file, never the host shell or runtime
        // process env. See [`crate::session::environment::DockerExecEnv`].
        let session = self.container_terminal_tmux_session_indexed(index)?;
        let is_new = !session.exists();
        if is_new {
            let session = tmux::Session::from_name(session.name());
            session.create_with_size_env_and_container_env(
                &self.project_path,
                Some(&cmd),
                size,
                &self.effective_profile(),
                &[],
                &env_info.env,
            )?;
            self.apply_container_terminal_tmux_options(index);
        }

        Ok(())
    }

    pub fn kill_container_terminal(&self) -> Result<()> {
        self.kill_container_terminal_indexed(0)
    }

    pub fn kill_container_terminal_indexed(&self, index: u32) -> Result<()> {
        let session = self.container_terminal_tmux_session_indexed(index)?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    /// Container counterpart of [`Self::kill_terminal_if_dead`].
    pub fn kill_container_terminal_if_dead(&self) -> Result<bool> {
        self.kill_container_terminal_if_dead_indexed(0)
    }

    pub fn kill_container_terminal_if_dead_indexed(&self, index: u32) -> Result<bool> {
        let session = self.container_terminal_tmux_session_indexed(index)?;
        if session.exists() && session.is_pane_dead() {
            let _ = session.kill();
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_terminal_autodetect_cmd_resolves_login_shell() {
        let cmd = CONTAINER_TERMINAL_AUTODETECT_CMD;
        // Resolution order: passwd entry first (authoritative, since docker exec
        // skips login(1) and so $SHELL is usually unset), then $SHELL, then
        // bash, sh. Each candidate is guarded by `command -v` so an unset, stale,
        // or non-executable value falls through rather than killing the pane.
        assert!(cmd.contains("getent passwd"));
        assert!(cmd.contains(r#"command -v "$SHELL""#));
        assert!(cmd.contains("command -v bash"));
        assert!(cmd.contains("command -v sh"));
        // Passwd is resolved ahead of $SHELL.
        assert!(cmd.find("getent passwd").unwrap() < cmd.find(r#"command -v "$SHELL""#).unwrap());
        // Login shell so profile/rc files load, matching the Host terminal tab.
        assert!(cmd.contains("-l"));
        // Single-quoted body: the embedded command substitution is evaluated by
        // the container's sh, not the host shell tmux spawns the session with.
        assert!(cmd.starts_with("sh -c '"));
    }

    #[test]
    fn test_has_terminal_false_by_default() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(!inst.has_terminal());
    }

    #[test]
    fn test_has_terminal_true_when_created() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.terminal_info = Some(TerminalInfo { created: true });
        assert!(inst.has_terminal());
    }

    #[test]
    fn test_terminal_info_none_means_no_terminal() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(inst.terminal_info.is_none());
        assert!(!inst.has_terminal());
    }

    #[test]
    fn test_terminal_info_created_false_means_no_terminal() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.terminal_info = Some(TerminalInfo { created: false });
        assert!(!inst.has_terminal());
    }

    mod kill_terminal_if_dead {
        use super::*;

        fn tmux_available() -> bool {
            crate::tmux::tmux_command()
                .arg("-V")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        /// Manually create a tmux session under `name` with `remain-on-exit on`
        /// so the session survives the inner command's exit. Used to simulate
        /// the dead-pane state without going through `start_terminal`, which
        /// would also apply unrelated tmux options.
        fn spawn_remain_on_exit(name: &str, cmd: &str) {
            let output = crate::tmux::tmux_command()
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    name,
                    "-x",
                    "80",
                    "-y",
                    "24",
                    cmd,
                    ";",
                    "set-option",
                    "-p",
                    "-t",
                    name,
                    "remain-on-exit",
                    "on",
                ])
                .output()
                .expect("tmux new-session");
            assert!(
                output.status.success(),
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            crate::tmux::refresh_session_cache();
        }

        fn cleanup(name: &str) {
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", name])
                .output();
            crate::tmux::refresh_session_cache();
        }

        #[test]
        #[serial_test::serial]
        fn returns_false_when_no_session() {
            if !tmux_available() {
                eprintln!("Skipping: tmux not available");
                return;
            }
            let inst = Instance::new("ktid_missing", "/tmp");
            crate::tmux::refresh_session_cache();
            assert!(!inst.kill_terminal_if_dead().unwrap());
        }

        #[test]
        #[serial_test::serial]
        fn returns_false_when_pane_alive() {
            if !tmux_available() {
                eprintln!("Skipping: tmux not available");
                return;
            }
            let inst = Instance::new("ktid_alive", "/tmp");
            let name = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
            spawn_remain_on_exit(&name, "sleep 30");
            // Give tmux a moment to register the pane.
            std::thread::sleep(std::time::Duration::from_millis(200));

            let result = inst.kill_terminal_if_dead();
            cleanup(&name);

            assert!(!result.unwrap(), "live pane should not trigger a kill");
        }

        #[test]
        #[serial_test::serial]
        fn kills_dead_pane_session() {
            if !tmux_available() {
                eprintln!("Skipping: tmux not available");
                return;
            }
            let inst = Instance::new("ktid_dead", "/tmp");
            let name = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);
            // `true` exits immediately; remain-on-exit keeps the session alive
            // with a dead pane (matches the production failure mode: shell
            // exited via Ctrl+D / `exit` / SIGHUP, session still listed).
            spawn_remain_on_exit(&name, "true");
            // Allow the pane to transition to dead.
            std::thread::sleep(std::time::Duration::from_millis(300));

            let session = inst.terminal_tmux_session().unwrap();
            assert!(
                session.exists(),
                "session should still exist via remain-on-exit"
            );
            assert!(
                session.is_pane_dead(),
                "pane should be dead after `true` exits"
            );

            let killed = inst.kill_terminal_if_dead().unwrap();
            assert!(
                killed,
                "kill_terminal_if_dead should return true for dead pane"
            );

            let session = inst.terminal_tmux_session().unwrap();
            assert!(!session.exists(), "session should be gone after kill");

            // Idempotent: second call on now-missing session returns false.
            assert!(
                !inst.kill_terminal_if_dead().unwrap(),
                "second call on missing session should return false"
            );

            cleanup(&name);
        }
    }
}
