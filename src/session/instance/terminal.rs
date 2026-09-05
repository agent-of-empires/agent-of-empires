//! Companion terminal panes, on the host and inside a container.

use super::*;

/// Command run inside the sandbox container for the web Container terminal tab.
///
/// Resolves the container user's preferred shell at spawn time, inside the
/// container. Known-compatible shells run in login mode so profile/rc files
/// load; other authorized shells run plain.
/// Resolution order: the passwd entry, `$SHELL`, bash, then sh. Each candidate
/// is resolved and validated inside the container as a regular executable
/// authorized shell. Passwd is read directly when `getent` is unavailable.
///
/// The script is evaluated by the container's `/bin/sh`, not the host shell tmux
/// uses to spawn the session, so the embedded `$()` runs in the container. The
/// host does not propagate its own `$SHELL` into the container, so this reads the
/// container's value, not the host's.
///
/// `@KNOWN_SHELLS@` and `@LOGIN_FLAG_SHELLS@` are substituted from
/// [`crate::session::environment`] so the container tab recognizes the same
/// shells, and makes the same login-mode call, as the host tab.
const CONTAINER_TERMINAL_AUTODETECT_SCRIPT: &str = r#"passwd_file=$1
shells_file=$2

lookup_shell() {
    wanted_uid=$1
    while IFS=: read -r _ _ entry_uid _ _ _ candidate || [ -n "$candidate" ]; do
        if [ "$entry_uid" = "$wanted_uid" ]; then
            printf "%s\n" "$candidate"
            return
        fi
    done
}

resolve_shell() {
    candidate=$1
    case "$candidate" in
        ""|-*) return 1 ;;
    esac

    case "$candidate" in
        */*) resolved=$candidate ;;
        *) resolved=$(command -v "$candidate" 2>/dev/null) || return 1 ;;
    esac
    case "$resolved" in
        /*) ;;
        */*)
            directory=${resolved%/*}
            filename=${resolved##*/}
            directory=$(CDPATH= cd "$directory" 2>/dev/null && pwd -P) || return 1
            resolved=$directory/$filename
            ;;
        *) return 1 ;;
    esac
    [ -f "$resolved" ] && [ -x "$resolved" ] || return 1

    case "${resolved##*/}" in
        @KNOWN_SHELLS@)
            printf "%s\n" "$resolved"
            return
            ;;
    esac

    if [ -r "$shells_file" ]; then
        while IFS= read -r allowed || [ -n "$allowed" ]; do
            case "$allowed" in
                ""|\#*) continue ;;
            esac
            if [ "$resolved" = "$allowed" ]; then
                printf "%s\n" "$resolved"
                return
            fi
        done < "$shells_file"
    fi
    return 1
}

uid=$(id -u 2>/dev/null || true)
if [ -z "$uid" ] && [ -r /proc/self/status ]; then
    while read -r key _ effective _; do
        if [ "$key" = "Uid:" ]; then
            uid=$effective
            break
        fi
    done < /proc/self/status
fi

passwd_shell=
if [ -n "$uid" ]; then
    if command -v getent >/dev/null 2>&1; then
        passwd_shell=$(getent passwd "$uid" 2>/dev/null | lookup_shell "$uid")
    fi
    if [ -z "$passwd_shell" ] && [ -r "$passwd_file" ]; then
        passwd_shell=$(lookup_shell "$uid" < "$passwd_file")
    fi
fi

shell=$(resolve_shell "$passwd_shell" || resolve_shell "${SHELL-}" || resolve_shell bash || resolve_shell /bin/sh) || {
    printf "%s\n" "No usable shell found in container" >&2
    exit 127
}
export SHELL=$shell
case "${shell##*/}" in
    @LOGIN_FLAG_SHELLS@) exec "$shell" -l ;;
    *) exec "$shell" ;;
esac
"#;

fn container_terminal_autodetect_command(passwd_file: &str, shells_file: &str) -> String {
    let script = CONTAINER_TERMINAL_AUTODETECT_SCRIPT
        .replace(
            "@KNOWN_SHELLS@",
            &crate::session::environment::known_shell_case_pattern(),
        )
        .replace(
            "@LOGIN_FLAG_SHELLS@",
            &crate::session::environment::login_flag_shell_case_pattern(),
        );
    format!(
        "/bin/sh -c {} aoe-container-terminal {} {}",
        shell_escape_script_word(&script),
        shell_escape_script_word(passwd_file),
        shell_escape_script_word(shells_file)
    )
}

fn container_terminal_exec_options(workdir: &str, env_args: &str) -> String {
    format!("-w {} {}", shell_escape_script_word(workdir), env_args)
}

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

        let resolver_command = container_terminal_autodetect_command("/etc/passwd", "/etc/shells");
        let cmd = container.exec_command(
            Some(&container_terminal_exec_options(
                &container_workdir,
                &env_part,
            )),
            &resolver_command,
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
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    fn write_executable(path: &std::path::Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn container_terminal_resolver_executes_only_usable_shells() {
        for invalid_kind in ["non_executable", "directory", "non_shell"] {
            let temp = tempfile::tempdir().unwrap();
            write_executable(
                &temp.path().join("getent"),
                r#"#!/bin/sh
printf 'test:x:2999:2999::/tmp:%s\n' "$PASSWD_SHELL"
"#,
            );
            write_executable(&temp.path().join("id"), "#!/bin/sh\nprintf 2999\n");

            let candidate = temp.path().join(invalid_kind);
            match invalid_kind {
                "non_executable" => std::fs::write(&candidate, "not executable").unwrap(),
                "directory" => std::fs::create_dir(&candidate).unwrap(),
                "non_shell" => write_executable(&candidate, "#!/bin/sh\necho WRONG_CANDIDATE\n"),
                _ => unreachable!(),
            }

            let resolver_command =
                container_terminal_autodetect_command("/etc/passwd", "/etc/shells");
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg(&resolver_command)
                .env("HOME", temp.path())
                .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
                .env("PASSWD_SHELL", &candidate)
                .env("SHELL", "/bin/sh")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"echo RESOLVED_SHELL\nexit\n")
                .unwrap();
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);

            assert!(
                output.status.success(),
                "resolver failed for {invalid_kind}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains("RESOLVED_SHELL"),
                "resolver executed {invalid_kind} instead of a shell: stdout={stdout}, stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!stdout.contains("WRONG_CANDIDATE"));
        }

        let temp = tempfile::tempdir().unwrap();
        let shell = temp.path().join("fish");
        write_executable(
            &shell,
            r#"#!/bin/sh
printf 'CUSTOM_SHELL %s %s\n' "$1" "$SHELL"
"#,
        );
        write_executable(
            &temp.path().join("getent"),
            r#"#!/bin/sh
printf 'test:x:2999:2999::/tmp:%s\n' "$PASSWD_SHELL"
"#,
        );
        write_executable(&temp.path().join("id"), "#!/bin/sh\nprintf 2999\n");
        let resolver_command = container_terminal_autodetect_command("/etc/passwd", "/etc/shells");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&resolver_command)
            .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
            .env("PASSWD_SHELL", &shell)
            .env_remove("SHELL")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains(&format!("CUSTOM_SHELL -l {}", shell.display())));
    }
    #[test]
    fn container_terminal_resolver_reads_passwd_without_getent() {
        for (case, passwd_ending, shells_ending) in [
            ("terminated", "\n", "\n"),
            ("unterminated_passwd", "", "\n"),
            ("unterminated_shells", "\n", ""),
        ] {
            let temp = tempfile::tempdir().unwrap();
            write_executable(&temp.path().join("id"), "#!/bin/sh\nprintf 2999\n");

            let passwd_shell = temp.path().join("custom-authorized-shell");
            write_executable(
                &passwd_shell,
                "#!/bin/sh\nprintf 'PASSWD argc=%s arg1=%s shell=%s\\n' \"$#\" \"${1-}\" \"$SHELL\"\n",
            );
            let fallback_shell = temp.path().join("bash");
            write_executable(&fallback_shell, "#!/bin/sh\nprintf 'WRONG_FALLBACK\\n'\n");

            let passwd_file = temp.path().join("passwd");
            std::fs::write(
                &passwd_file,
                format!(
                    "test:x:2999:2999::/tmp:{}{passwd_ending}",
                    passwd_shell.display()
                ),
            )
            .unwrap();
            let shells_file = temp.path().join("shells");
            std::fs::write(
                &shells_file,
                format!("{}{shells_ending}", passwd_shell.display()),
            )
            .unwrap();

            let resolver_command = container_terminal_autodetect_command(
                passwd_file.to_str().unwrap(),
                shells_file.to_str().unwrap(),
            );
            let output = Command::new("/bin/sh")
                .args(["-c", &resolver_command])
                .env_clear()
                .env("HOME", temp.path())
                .env("PATH", temp.path())
                .env("SHELL", &fallback_shell)
                .output()
                .unwrap();

            assert!(
                output.status.success(),
                "{case}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty(), "{case}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("PASSWD argc=0 arg1= shell={}\n", passwd_shell.display()),
                "{case}"
            );
        }
    }

    #[test]
    fn container_terminal_command_preserves_runtime_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_driver = r#"#!/bin/sh
[ "$1" = exec ] || exit 90
shift
[ "$1" = -it ] || exit 91
shift
[ "$1" = -w ] || exit 92
printf '%s' "$2" > "$ARGS_FILE"
shift 2
if [ "${1-}" = --env-file ]; then
    [ "$2" = /dev/fd/9 ] || exit 93
    shift 2
fi
[ "$1" = test-container ] || exit 94
shift
exec /usr/bin/env -i PATH="$TARGET_PATH" SHELL="$FALLBACK_SHELL" "$@"
"#;
        for binary in ["docker", "podman", "container"] {
            write_executable(&temp.path().join(binary), runtime_driver);
        }
        write_executable(&temp.path().join("id"), "#!/bin/sh\nprintf 2999\n");
        let fallback_shell = temp.path().join("bash");
        write_executable(&fallback_shell, "#!/bin/sh\nprintf 'WRONG_FALLBACK\\n'\n");

        let fixtures = temp.path().join("fixtures'quoted");
        std::fs::create_dir(&fixtures).unwrap();
        let passwd_shell = fixtures.join("custom-authorized-shell");
        write_executable(
            &passwd_shell,
            "#!/bin/sh\nprintf 'RUNTIME argc=%s shell=%s\\n' \"$#\" \"$SHELL\"\n",
        );
        let passwd_file = fixtures.join("passwd");
        std::fs::write(
            &passwd_file,
            format!("test:x:2999:2999::/tmp:{}\n", passwd_shell.display()),
        )
        .unwrap();
        let shells_file = fixtures.join("shells");
        std::fs::write(&shells_file, format!("{}\n", passwd_shell.display())).unwrap();
        let resolver_command = container_terminal_autodetect_command(
            passwd_file.to_str().unwrap(),
            shells_file.to_str().unwrap(),
        );

        let injection_marker = temp.path().join("injected");
        let workdir = format!(
            "/workspace/project\nline\rcarriage' ; printf PWNED > {}; #",
            injection_marker.display()
        );
        let options = container_terminal_exec_options(&workdir, "--env-file /dev/fd/9 ");

        for (binary, runtime) in [
            ("docker", crate::containers::ContainerRuntime::docker()),
            ("podman", crate::containers::ContainerRuntime::podman()),
            (
                "container",
                crate::containers::ContainerRuntime::apple_container(),
            ),
        ] {
            let args_file = temp.path().join(format!("args-{binary}"));
            let command = runtime.exec_command("test-container", Some(&options), &resolver_command);
            let output = Command::new("/bin/sh")
                .args(["-c", &command])
                .env_clear()
                .env("PATH", temp.path())
                .env("ARGS_FILE", &args_file)
                .env("TARGET_PATH", temp.path())
                .env("FALLBACK_SHELL", &fallback_shell)
                .output()
                .unwrap();

            assert!(
                output.status.success(),
                "{binary}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read(&args_file).unwrap(), workdir.as_bytes());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("RUNTIME argc=0 shell={}\n", passwd_shell.display()),
                "{binary}"
            );
        }
        assert!(!injection_marker.exists());
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
