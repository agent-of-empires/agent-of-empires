//! Spawning the detached runner process and waiting for its control socket.

use tracing::{info, warn};

use super::errors::AcpError;
use super::resolve_command::{resolve_agent_command, Unresolved};
use super::session_sandbox::{build_sandbox_docker_argv, SessionSandbox};
use super::spawn::{
    apply_claude_store_route, apply_env_filter, host_environment_denyreason, prepend_path_dirs,
    SpawnConfig,
};

/// Deadline for the runner socket to appear. 10s suffices in production, but
/// a debug-build cold start under CI load blows past it deterministically, so
/// debug builds honor `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS`.
pub(super) fn runner_socket_deadline() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            // Floor so a `...=0` typo cannot fail the dial before its first
            // retry, which surfaces as "does not speak control protocol".
            return std::time::Duration::from_millis(ms.max(100));
        }
    }
    std::time::Duration::from_secs(10)
}

/// Fault injection for the #1890 e2e: the first N fresh-spawn handshakes fail
/// after the runner is up but before the daemon records a worker, leaving a
/// live registered runner the daemon never adopted. Each call consumes one
/// budgeted failure; debug builds only.
#[cfg(debug_assertions)]
pub(super) fn take_injected_fresh_handshake_failure() -> bool {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::OnceLock;
    static REMAINING: OnceLock<AtomicI64> = OnceLock::new();
    let remaining = REMAINING.get_or_init(|| {
        let n = std::env::var("AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0);
        AtomicI64::new(n)
    });
    // `try_update` needs Rust 1.99, newer than the MSRV and the Nix toolchain.
    #[allow(deprecated)]
    let claimed = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        (n > 0).then_some(n - 1)
    });
    claimed.is_ok()
}

/// Prepends the scope `prefix` to `cmd`. The runner env is cleared, so the
/// user-manager variables `systemd-run` needs are added for it and unset again
/// with `env -u` in front of the runner, next to the restore of the variable
/// `systemd-run` injects. Stdio and `pre_exec` are not carried over: set them
/// on the result. A program containing `=` stays unscoped, since `env` would
/// read it as an assignment.
pub(crate) fn wrap_in_scope(
    cmd: tokio::process::Command,
    prefix: &crate::process::systemd::ScopePrefix,
) -> tokio::process::Command {
    const BUS_VARS: [&str; 2] = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];
    let original = cmd.as_std();
    if original.get_program().to_string_lossy().contains('=') {
        tracing::debug!(target: "process.systemd", "runner path contains '='; starts unscoped");
        return cmd;
    }
    let restore = prefix.with_env_restore();
    let Some((program, prefix_args)) = restore.split_first() else {
        return cmd;
    };
    let added: Vec<(&str, std::ffi::OsString)> = BUS_VARS
        .iter()
        .filter(|var| !original.get_envs().any(|(k, _)| k == **var))
        .filter_map(|var| std::env::var_os(var).map(|v| (*var, v)))
        .collect();
    let injected = crate::process::systemd::INJECTED_VAR;
    let own_injected = original
        .get_envs()
        .find_map(|(k, v)| (k == injected).then_some(v).flatten());

    let mut wrapped = tokio::process::Command::new(program);
    wrapped.args(prefix_args);
    for (var, _) in &added {
        wrapped.arg("-u").arg(var);
    }
    if let Some(value) = own_injected {
        let mut assignment = std::ffi::OsString::from(format!("{injected}="));
        assignment.push(value);
        wrapped.arg(assignment);
    }
    wrapped
        .arg(original.get_program())
        .args(original.get_args());
    wrapped.env_clear();
    for (key, value) in original.get_envs() {
        if let Some(value) = value {
            wrapped.env(key, value);
        }
    }
    wrapped.envs(added);
    if let Some(dir) = original.get_current_dir() {
        wrapped.current_dir(dir);
    }
    wrapped
}

/// The runner log opened for appending, with the runner's own 0600 mode, or
/// null when it cannot be opened.
fn append_to_log(path: &std::path::Path) -> std::process::Stdio {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Ok(file) => file.into(),
        Err(e) => {
            tracing::debug!(target: "process.systemd", "runner log {}: {e}", path.display());
            std::process::Stdio::null()
        }
    }
}

/// A Worker started for a bare adapter name that resolved nowhere can only exit
/// before its Hello, which loses the cause (#4576). `unresolved` is `None` when
/// the name resolved or a sandbox resolves it inside its container; paths and
/// `${placeholder}` commands resolve elsewhere. The adapter searches the last
/// `PATH` in `agent_environment`, else the inherited one, from its cwd, so
/// relative entries resolve against the project.
fn refuse_unresolved_adapter(
    config: &SpawnConfig,
    unresolved: Option<&Unresolved>,
    agent_environment: &[(String, String)],
    inherited_path: Option<&std::ffi::OsStr>,
) -> Result<(), AcpError> {
    let Some(reason) = unresolved.filter(|reason| **reason != Unresolved::NotBare) else {
        return Ok(());
    };
    let path = agent_environment
        .iter()
        .rev()
        .find(|(key, _)| key == "PATH")
        .map(|(_, path)| std::ffi::OsStr::new(path))
        .or(inherited_path);
    if path
        .is_some_and(|path| which::which_in(&config.spec.command, Some(path), &config.cwd).is_ok())
    {
        return Ok(());
    }
    Err(AcpError::unresolved_adapter_spawn_error(
        &std::io::Error::from(std::io::ErrorKind::NotFound),
        config,
        reason,
    ))
}

/// The runner owns the agent subprocess and outlives the daemon; its Child is
/// transferred to the launch lease so teardown remains owned across cancellation.
pub(super) async fn spawn_runner_detached(
    config: &SpawnConfig,
    socket_path: &std::path::Path,
    session_id: String,
    session_sandbox: Option<&SessionSandbox>,
) -> Result<(u32, Option<crate::session::ExecutionBinding>), AcpError> {
    let current_exe =
        std::env::current_exe().map_err(|e| AcpError::Spawn(format!("current_exe: {e}")))?;
    let log_path = crate::process::worker_registry::log_path_for(&session_id)
        .map_err(|e| AcpError::Spawn(format!("log path: {e}")))?;
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // A sandboxed session wraps the agent in `docker exec`, so host PATH
    // resolution is skipped: the container's own PATH resolves the binary.
    let sandbox_argv = match (&config.sandbox_info, session_sandbox) {
        (Some(sandbox), Some(handle)) => {
            let argv = build_sandbox_docker_argv(
                config,
                sandbox,
                handle.container_workdir.to_string_lossy().as_ref(),
            )?;
            info!(
                target: "acp.protocol.spawn",
                session = %session_id,
                container = %sandbox.container_name,
                container_id = sandbox.container_id.as_deref().unwrap_or("?"),
                image = %sandbox.image,
                workdir = %handle.container_workdir.display(),
                docker = %argv.docker_binary,
                "docker wrap applied"
            );
            Some(argv)
        }
        (Some(_), None) => {
            return Err(AcpError::Spawn(
                "sandbox_info set but SessionSandbox handle missing; \
                 SessionSandbox::from_info must run before spawn_runner_detached"
                    .into(),
            ));
        }
        (None, _) => {
            info!(
                target: "acp.protocol.spawn",
                session = %session_id,
                "docker wrap skipped (no sandbox_info)"
            );
            None
        }
    };

    // Resolve against PATH plus the node-manager dirs so the runner finds the
    // binary even when the daemon's frozen PATH does not carry it (#1048).
    let resolution = if sandbox_argv.is_some() {
        None
    } else {
        // A get_app_dir failure only costs the bundled-adapter lookup; PATH
        // and the node-manager scan still run.
        let app_dir = crate::session::get_app_dir().ok();
        Some(resolve_agent_command(
            &config.spec.command,
            app_dir.as_deref(),
        ))
    };
    // Trusted `Config.environment`, then the provider pick that outranks it, for
    // the adapter only: they ride a carrier key the runner strips and applies to
    // its child, because HOME, PATH or XDG_CONFIG_HOME set on the runner itself
    // would move its worker registry (#1383) or change which binary it loads.
    let host_environment: Vec<(String, String)> = config
        .host_environment
        .iter()
        .chain(&config.provider_routing)
        .filter(|(key, _)| match host_environment_denyreason(key) {
            Some(reason) => {
                warn!(
                    target: "acp",
                    key = %key,
                    reason,
                    "rejecting configured host environment key",
                );
                false
            }
            None => true,
        })
        .cloned()
        .collect();
    refuse_unresolved_adapter(
        config,
        resolution.as_ref().and_then(|r| r.as_ref().err()),
        &host_environment,
        std::env::var_os("PATH").as_deref(),
    )?;
    let resolved = resolution.and_then(Result::ok);
    let (spawn_command, extra_path_dirs): (String, Vec<std::path::PathBuf>) =
        match (&sandbox_argv, &resolved) {
            (Some(s), _) => (s.docker_binary.clone(), Vec::new()),
            (None, Some(r)) => (
                r.path.to_string_lossy().into_owned(),
                r.prepend_paths.clone(),
            ),
            (None, None) => (config.spec.command.clone(), Vec::new()),
        };

    let prefix = {
        let id = session_id.clone();
        let profile = config.source_profile.clone().unwrap_or_default();
        tokio::task::spawn_blocking(move || {
            crate::process::systemd::scope_prefix_for(&id, &profile)
        })
        .await
        .ok()
        .flatten()
    }
    .filter(|_| !current_exe.to_string_lossy().contains('='));
    let scope = prefix.as_ref().map_or(
        crate::process::worker_registry::ScopeOwnership::Unscoped,
        |prefix| prefix.ownership(),
    );
    let mut cmd = tokio::process::Command::new(&current_exe);
    cmd.arg("__acp-runner")
        .arg("--socket")
        .arg(socket_path)
        .arg("--session-id")
        .arg(&session_id)
        .arg("--agent-name")
        .arg(&config.spec.command)
        .arg("--agent-key")
        .arg(&config.agent_key)
        .arg("--cwd")
        .arg(&config.cwd);
    if !config.additional_dirs.is_empty() {
        cmd.arg("--additional-dirs").arg(
            config
                .additional_dirs
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    let provider_keys: Vec<&str> = config
        .provider_env
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    if !provider_keys.is_empty() {
        cmd.arg("--provider-env-keys").arg(provider_keys.join(","));
    }
    if let Some(profile) = config.source_profile.as_deref().filter(|s| !s.is_empty()) {
        cmd.arg("--source-profile").arg(profile);
    }
    if let Some(stored) = &config.stored_acp_session_id {
        cmd.arg("--stored-acp-session-id").arg(stored);
    }
    cmd.arg("--generation").arg(config.generation.to_string());
    if let Some(admission) = config
        .launch_lease
        .as_ref()
        .and_then(|lease| lease.admission())
    {
        cmd.arg("--lifecycle-generation")
            .arg(admission.generation.to_string());
    }
    if let crate::process::worker_registry::ScopeOwnership::Owned { unit } = &scope {
        cmd.arg("--systemd-scope").arg(unit);
    }
    cmd.arg("--");
    if let Some(s) = &sandbox_argv {
        cmd.arg(&s.docker_binary);
        for a in &s.docker_args {
            cmd.arg(a);
        }
    } else {
        // Pass the resolved absolute path (or fall back to the bare command).
        // The runner spawns whatever it receives, so an absolute path bypasses
        // any PATH lookup inside the runner.
        cmd.arg(&spawn_command);
        for a in &config.spec.args {
            cmd.arg(a);
        }
    }

    // The runner inherits this env when it spawns the agent, so one filter
    // pass covers both. AOE_TOKEN is stripped here and reaches neither.
    cmd.env_clear();
    apply_env_filter(cmd.as_std_mut(), config, &[]);
    apply_claude_store_route(cmd.as_std_mut(), config);
    #[cfg(debug_assertions)]
    if let Ok(interval) = std::env::var("AOE_ACP_WATCHDOG_POLL_MS") {
        cmd.env("AOE_ACP_WATCHDOG_POLL_MS", interval);
    }
    if !host_environment.is_empty() {
        let encoded = serde_json::to_string(&host_environment)
            .map_err(|e| AcpError::Spawn(format!("encode host environment: {e}")))?;
        cmd.env(crate::process::runner::ACP_AGENT_ENV, encoded);
    }
    if let Some(s) = &sandbox_argv {
        // docker reads each `-e KEY` value from its own process env, so set
        // them on the runner, which is docker's parent.
        for (key, value) in &s.inherit_env {
            cmd.env(key, value);
        }
    } else if let Some(dir) = &config.artifact_dir {
        // On the host, so point at the host dir; the sandbox path exports the
        // fixed container mount in build_sandbox_docker_argv (#2587).
        cmd.env(crate::session::artifacts::ARTIFACT_DIR_ENV, dir);
    }
    if !config.uses_native_prime() {
        if let Some(lease) = &config.launch_lease {
            lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::NotPrime);
        }
    }
    if config.uses_native_prime() {
        let mut snapshot = std::process::Command::new(&spawn_command);
        snapshot.envs(
            cmd.as_std()
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        );
        let runtime = super::spawn::prime_runtime_for_spawn(
            config,
            &spawn_command,
            &snapshot,
            &host_environment,
            sandbox_argv.as_ref(),
        )?
        .ok_or_else(|| AcpError::Spawn("Prime runtime missing".into()))?;
        if let Some(lease) = &config.launch_lease {
            lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::Owned {
                runtime: Box::new(runtime.clone()),
            });
        }
        let encoded =
            serde_json::to_string(&runtime).map_err(|e| AcpError::Spawn(e.to_string()))?;
        let original: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(std::ffi::OsStr::to_owned)
            .collect();
        let separator = original
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| AcpError::Spawn("runner argument separator missing".into()))?;

        let mut owned = tokio::process::Command::new(&current_exe);
        owned
            .args(&original[..separator])
            .arg("--prime-runtime")
            .arg(&encoded)
            .arg("--")
            .arg(&current_exe)
            .arg("__prime-agent")
            .arg("--runtime")
            .arg(&encoded)
            .arg("--")
            .args(&original[separator + 1..])
            .arg("--daemon-socket")
            .arg(runtime.socket());
        owned.env_clear().envs(
            cmd.as_std()
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        );
        cmd = owned;
    }
    if !extra_path_dirs.is_empty() {
        // Prepend the resolved bin dirs so the adapter and its
        // `#!/usr/bin/env node` shim resolve against the same install.
        let current = std::env::var_os("PATH").unwrap_or_default();
        cmd.env("PATH", prepend_path_dirs(&current, &extra_path_dirs));
    }

    // Reads the runner env, so it runs before the scope wrapper adds its own.
    let native_store = super::spawn::native_store_snapshot(config, cmd.as_std(), &host_environment);
    let mut cmd = match &prefix {
        Some(prefix) => wrap_in_scope(cmd, prefix),
        None => cmd,
    };

    // Its own session leader, so a SIGTERM to the daemon's group does not
    // cascade; the runner installs its own handlers.
    #[cfg(unix)]
    {
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }

    // The runner writes its own log file. Inheriting our stdio would put
    // per-session noise in debug.log and leave the runner reading EOF on its
    // own stdin once the daemon dies. A scoped start sends stderr to that log
    // so a failing `systemd-run` leaves its reason there.
    let stderr = match &prefix {
        Some(_) => append_to_log(&log_path),
        None => std::process::Stdio::null(),
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr);

    info!(
        target: "acp.protocol.spawn",
        session = %session_id,
        socket = %socket_path.display(),
        runner = %current_exe.display(),
        agent = %config.spec.command,
        resolved = %spawn_command,
        "spawning detached structured view runner"
    );

    let lease = config.launch_lease.clone();
    let admission = lease.as_ref().and_then(|lease| lease.admission());
    let generation = config.generation;
    let pid = crate::acp::supervisor::with_durable_launch(admission, session_id, move || {
        let mut birth = lease.as_ref().map(|lease| lease.lock_birth());
        if lease
            .as_ref()
            .is_some_and(|lease| lease.cancellation().is_cancelled())
        {
            return Err(AcpError::Spawn(
                "launch cancelled before physical spawn".into(),
            ));
        }
        if let Some(scopes) = birth.as_mut() {
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|error| AcpError::Spawn(format!("spawn runner: {error}")))?;
        let pid = child
            .id()
            .ok_or_else(|| AcpError::Spawn("runner exited before it could be identified".into()))?;
        // Transfer reaping before the blocking result can be dropped.
        if let Some(lease) = &lease {
            lease.record_spawned_child(
                child,
                crate::acp::runner_lifecycle::RunnerIdentity {
                    pid,
                    generation,
                    birth_fingerprint: crate::process::process_birth_identity(pid)
                        .ok()
                        .flatten()
                        .map(|identity| identity.fingerprint()),
                },
            );
        } else {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Ok(pid)
    })
    .await
    .map_err(|error| AcpError::Spawn(error.to_string()))?;
    Ok((pid, native_store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::systemd::ScopePrefix;
    use crate::session::test_support::EnvGuard;
    use std::ffi::{OsStr, OsString};

    const XDG: &str = "XDG_RUNTIME_DIR";
    const BUS: &str = "DBUS_SESSION_BUS_ADDRESS";
    const ENV: &str = "/usr/bin/env";

    fn prefix() -> ScopePrefix {
        ScopePrefix {
            argv: ["/usr/bin/systemd-run", "--user", "--scope", "--"]
                .map(String::from)
                .to_vec(),
            env_bin: ENV.to_string(),
        }
    }

    fn agent() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("agent");
        cmd.arg("--flag")
            .arg("value")
            .env_clear()
            .env("A", "1")
            .env_remove("B")
            .current_dir("/example/cwd");
        cmd
    }

    fn argv(cmd: &tokio::process::Command) -> Vec<OsString> {
        let std = cmd.as_std();
        std::iter::once(std.get_program())
            .chain(std.get_args())
            .map(OsStr::to_os_string)
            .collect()
    }

    fn envs(cmd: &tokio::process::Command) -> Vec<(OsString, Option<OsString>)> {
        let mut v: Vec<_> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(OsStr::to_os_string)))
            .collect();
        v.sort();
        v
    }

    fn env_bin() -> String {
        which::which("env")
            .expect("env on PATH")
            .to_string_lossy()
            .into_owned()
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn pair(k: &str, v: &str) -> (OsString, Option<OsString>) {
        (k.into(), Some(v.into()))
    }

    #[test]
    fn wrap_in_scope_prepends_prefix_and_restores_the_agent_env() {
        // (aoe XDG, aoe BUS, original carries XDG, expected unset vars)
        let cases: [(Option<&str>, Option<&str>, bool, &[&str]); 4] = [
            (
                Some("/run/user/example"),
                Some("unix:path=/x"),
                false,
                &[XDG, BUS],
            ),
            (
                Some("/run/user/example"),
                Some("unix:path=/x"),
                true,
                &[BUS],
            ),
            (Some("/run/user/example"), None, false, &[XDG]),
            (None, None, false, &[]),
        ];
        for (xdg, bus, carries_xdg, unset) in cases {
            let _guard = match (xdg, bus) {
                (Some(x), Some(b)) => EnvGuard::set(&[(XDG, x), (BUS, b)]),
                (Some(x), None) => EnvGuard::unset(&[BUS]).and_set(XDG, x),
                _ => EnvGuard::unset(&[XDG, BUS]),
            };
            let mut original = agent();
            if carries_xdg {
                original.env(XDG, "/run/user/own");
            }
            let wrapped = wrap_in_scope(original, &prefix());
            let label = format!("{xdg:?} {bus:?} carries_xdg={carries_xdg}");

            let mut want = prefix().argv;
            want.push(ENV.to_string());
            for var in ["INVOCATION_ID"].iter().chain(unset) {
                want.extend(["-u".to_string(), var.to_string()]);
            }
            want.extend(["agent", "--flag", "value"].map(String::from));
            assert_eq!(
                argv(&wrapped),
                os(&want.iter().map(String::as_str).collect::<Vec<_>>()),
                "{label}"
            );

            let mut want_env = vec![pair("A", "1")];
            if carries_xdg {
                want_env.push(pair(XDG, "/run/user/own"));
            }
            for var in unset {
                let value = if *var == XDG { xdg } else { bus };
                want_env.push(pair(var, value.unwrap()));
            }
            want_env.sort();
            assert_eq!(envs(&wrapped), want_env, "{label}: B stays removed");
            assert_eq!(
                wrapped.as_std().get_current_dir(),
                Some(std::path::Path::new("/example/cwd")),
                "{label}"
            );
        }
    }

    /// `env` would take a program path containing `=` for an assignment.
    #[test]
    fn wrap_in_scope_skips_a_program_with_an_equals_sign() {
        let original = || {
            let mut cmd = tokio::process::Command::new("/opt/a=b/aoe");
            cmd.arg("__acp-runner").env_clear().env("A", "1");
            cmd
        };
        let wrapped = wrap_in_scope(original(), &prefix());
        assert_eq!(argv(&wrapped), argv(&original()));
        assert_eq!(envs(&wrapped), envs(&original()));
    }

    /// Runs the wrapped command with `env` standing in for `systemd-run` and
    /// injecting `INVOCATION_ID` like it: the innermost `env` must see exactly
    /// the original, cleared environment.
    #[tokio::test]
    async fn wrapped_agent_sees_exactly_its_original_env() {
        let env = env_bin();
        let fake_systemd_run = ScopePrefix {
            argv: vec![env.clone(), "INVOCATION_ID=injected".to_string()],
            env_bin: env.clone(),
        };
        for own in [None, Some("own")] {
            let mut original = tokio::process::Command::new(&env);
            original.env_clear().env("A", "1");
            if let Some(own) = own {
                original.env("INVOCATION_ID", own);
            }
            let mut wrapped = {
                let _guard = EnvGuard::set(&[(XDG, "/run/user/example"), (BUS, "unix:path=/x")]);
                wrap_in_scope(original, &fake_systemd_run)
            };
            let out = wrapped.output().await.expect("run wrapped env");
            assert!(out.status.success(), "{out:?}");
            let mut seen: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(String::from)
                .collect();
            seen.sort();
            let mut want = vec!["A=1".to_string()];
            want.extend(own.map(|v| format!("INVOCATION_ID={v}")));
            assert_eq!(seen, want, "own INVOCATION_ID {own:?}");
        }
    }

    /// #4576: an unresolved bare adapter name fails before a Worker starts,
    /// naming that adapter's own install command.
    #[test]
    fn refuse_unresolved_adapter_cases() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = tempfile::tempdir().unwrap();
        // Only a PATH entry for this directory finds the adapter. As the
        // relative entry `agent-bin`, it resolves from the project cwd, not the
        // test process's.
        let agent_bin = cwd.path().join("agent-bin");
        std::fs::create_dir(&agent_bin).unwrap();
        let adapter = agent_bin.join("codex-acp");
        std::fs::write(&adapter, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let empty_bin = cwd.path().join("empty-bin");
        std::fs::create_dir(&empty_bin).unwrap();
        let (found, empty) = (agent_bin.to_str().unwrap(), empty_bin.to_str().unwrap());
        let hint = |cmd: &str| Some(crate::acp::install_hints::install_hint_for(cmd).unwrap());
        let missing = Some(Unresolved::NotFound);
        let stale = "aoe acp doctor --fix --adapter aoe-agent";
        let old_node = "needs Node 22.6 or newer to run its sources; found v22.1.0. Upgrade Node";
        let cases: [(
            &str,
            Option<Unresolved>,
            &[&str],
            Option<&str>,
            Option<&str>,
        ); 15] = [
            // (command, why it didn't resolve, configured PATHs, inherited PATH, expected error text)
            ("codex-acp", missing.clone(), &[], None, hint("codex-acp")),
            (
                "claude-agent-acp",
                missing.clone(),
                &[],
                None,
                hint("claude-agent-acp"),
            ),
            (
                "aoe-test-unknown-adapter",
                missing.clone(),
                &[],
                None,
                Some("binary `aoe-test-unknown-adapter` not found"),
            ),
            // Resolved, or sandboxed.
            ("codex-acp", None, &[], None, None),
            (
                "/opt/agents/codex-acp",
                Some(Unresolved::NotBare),
                &[],
                None,
                None,
            ),
            // A bundled copy aoe refuses needs a reinstall or a newer Node, not an install.
            (
                "aoe-agent",
                Some(Unresolved::StaleBundle),
                &[],
                None,
                Some(stale),
            ),
            (
                "aoe-agent",
                Some(Unresolved::NodeTooOld("v22.1.0".into())),
                &[],
                None,
                Some(old_node),
            ),
            ("codex-acp", missing.clone(), &[found], None, None),
            (
                "codex-acp",
                missing.clone(),
                &[empty],
                None,
                hint("codex-acp"),
            ),
            // The last configured PATH wins, as the runner applies them in order.
            (
                "codex-acp",
                missing.clone(),
                &[found, empty],
                None,
                hint("codex-acp"),
            ),
            ("codex-acp", missing.clone(), &[], Some("agent-bin"), None),
            (
                "codex-acp",
                missing.clone(),
                &[empty],
                Some(found),
                hint("codex-acp"),
            ),
            (
                "codex-acp",
                missing.clone(),
                &[],
                Some(empty),
                hint("codex-acp"),
            ),
            // A copy on the adapter's PATH wins over a refused bundle.
            (
                "codex-acp",
                Some(Unresolved::StaleBundle),
                &[found],
                None,
                None,
            ),
            (
                "codex-acp",
                Some(Unresolved::NodeTooOld("v22.1.0".into())),
                &[empty],
                None,
                Some("needs Node 22.6 or newer"),
            ),
        ];
        for (command, unresolved, configured, inherited, expected) in cases {
            let label = format!(
                "{command} unresolved={unresolved:?} configured={configured:?} inherited={inherited:?}"
            );
            let mut config =
                super::super::test_helpers::env_test_spawn_config(cwd.path().to_path_buf());
            config.spec.command = command.into();
            let environment: Vec<(String, String)> = configured
                .iter()
                .map(|path| ("PATH".to_string(), path.to_string()))
                .collect();
            // The runner hands the adapter this list; the error reads the same config.
            config.base_host_environment = environment.clone();
            config.host_environment = environment.clone();
            let got = refuse_unresolved_adapter(
                &config,
                unresolved.as_ref(),
                &environment,
                inherited.map(std::ffi::OsStr::new),
            )
            .err()
            .map(|e| match e {
                AcpError::Spawn(msg) => msg,
                other => panic!("{label}: unexpected {other:?}"),
            });
            match (&got, expected) {
                (Some(msg), Some(want)) => assert!(msg.contains(want), "{label}: {msg}"),
                _ => assert_eq!(got.is_some(), expected.is_some(), "{label}: {got:?}"),
            }
            // With a configured PATH, the advice is about that PATH.
            if let (Some(msg), Some(Unresolved::NotFound)) = (&got, &unresolved) {
                if !configured.is_empty() {
                    assert!(
                        msg.contains("`environment` sets for the agent"),
                        "{label}: {msg}"
                    );
                    assert!(!msg.contains("restart `aoe serve`"), "{label}: {msg}");
                }
            }
            if matches!(
                unresolved,
                Some(Unresolved::StaleBundle | Unresolved::NodeTooOld(_))
            ) {
                let msg = got.unwrap_or_default();
                assert!(!msg.contains("Install with:"), "{label}: {msg}");
            }
        }
        // Each adapter gets its own command, not another adapter's.
        let mut config =
            super::super::test_helpers::env_test_spawn_config(cwd.path().to_path_buf());
        config.spec.command = "codex-acp".into();
        let msg = format!(
            "{:?}",
            refuse_unresolved_adapter(&config, Some(&Unresolved::NotFound), &[], None)
        );
        assert!(!msg.contains(hint("claude-agent-acp").unwrap()), "{msg}");
    }

    /// #4576: the refusal leaves no Worker record, socket or log behind.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_refuses_an_unresolved_adapter_before_starting_a_worker() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let cwd = tempfile::tempdir().unwrap();
        let session = format!("aoe-test-missing-adapter-{}", std::process::id());
        let socket = crate::process::worker_registry::socket_path_for(&session).unwrap();
        let mut config =
            super::super::test_helpers::env_test_spawn_config(cwd.path().to_path_buf());
        config.spec.command = format!("aoe-test-no-such-adapter-{}", std::process::id());
        config.socket_path = Some(socket.clone());
        let mut lifecycle = crate::acp::runner_lifecycle::LifecycleTable::new(1);
        let lease = lifecycle
            .admit(&session, crate::acp::runner_lifecycle::ResumeKind::Spawn)
            .unwrap();
        config.launch_lease = Some(lease.clone());

        let err = super::super::AcpClient::spawn(
            config,
            crate::acp::state::AcpSessionId(session.clone()),
        )
        .await
        .err()
        .expect("spawn must fail");
        assert!(
            matches!(&err, AcpError::Spawn(msg) if msg.contains("not found on the daemon's PATH")),
            "{err:?}"
        );
        for path in [
            socket.clone(),
            crate::process::worker::control_socket_sibling(&socket),
            crate::process::worker_registry::log_path_for(&session).unwrap(),
            crate::process::worker_registry::record_path(&session).unwrap(),
        ] {
            assert!(!path.exists(), "left behind: {}", path.display());
        }
        assert!(lifecycle.abandon(&lease), "the failed producer must settle");
        lifecycle
            .admit(&session, crate::acp::runner_lifecycle::ResumeKind::Spawn)
            .expect("adapter refusal must not strand the session reservation");
    }
}
