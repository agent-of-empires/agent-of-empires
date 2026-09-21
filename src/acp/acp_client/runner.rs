//! Spawning the detached runner process and waiting for its control socket.

use tracing::{info, warn};

use super::errors::AcpError;
use super::resolve_command::resolve_agent_command;
use super::session_sandbox::{build_sandbox_docker_argv, SessionSandbox};
use super::spawn::{apply_env_filter, host_environment_denyreason, prepend_path_dirs, SpawnConfig};

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
    remaining
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n > 0).then_some(n - 1)
        })
        .is_ok()
}

/// The runner owns the agent subprocess and outlives the daemon, so no `Child`
/// handle is kept: the daemon reaches it over the unix socket, and the OS keeps
/// it alive across `aoe serve` restarts.
pub(super) fn spawn_runner_detached(
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
    let resolved = if sandbox_argv.is_some() {
        None
    } else {
        // A get_app_dir failure only costs the bundled-adapter lookup; PATH
        // and the node-manager scan still run.
        let app_dir = crate::session::get_app_dir().ok();
        resolve_agent_command(&config.spec.command, app_dir.as_deref())
    };
    let (spawn_command, extra_path_dirs): (String, Vec<std::path::PathBuf>) =
        match (&sandbox_argv, &resolved) {
            (Some(s), _) => (s.docker_binary.clone(), Vec::new()),
            (None, Some(r)) => (
                r.path.to_string_lossy().into_owned(),
                r.prepend_paths.clone(),
            ),
            (None, None) => (config.spec.command.clone(), Vec::new()),
        };

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
    #[cfg(debug_assertions)]
    if let Ok(interval) = std::env::var("AOE_ACP_WATCHDOG_POLL_MS") {
        cmd.env("AOE_ACP_WATCHDOG_POLL_MS", interval);
    }
    // Trusted `Config.environment` for the adapter only, riding one reserved
    // carrier key (JSON `[[key, value], ...]`) that the runner strips and
    // applies to its child. HOME / PATH / XDG_CONFIG_HOME are legal entries
    // here, and setting those on the runner itself would move the
    // worker-registry path it writes (#1383) or change which binary it loads.
    let host_environment: Vec<(String, String)> = config
        .host_environment
        .iter()
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
    if !extra_path_dirs.is_empty() {
        // Prepend the resolved bin dirs so the adapter and its
        // `#!/usr/bin/env node` shim resolve against the same install.
        let current = std::env::var_os("PATH").unwrap_or_default();
        cmd.env("PATH", prepend_path_dirs(&current, &extra_path_dirs));
    }

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
    // own stdin once the daemon dies.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    info!(
        target: "acp.protocol.spawn",
        session = %session_id,
        socket = %socket_path.display(),
        runner = %current_exe.display(),
        agent = %config.spec.command,
        resolved = %spawn_command,
        "spawning detached structured view runner"
    );

    let native_store = super::spawn::native_store_snapshot(config, cmd.as_std(), &host_environment);
    let mut child = cmd.spawn().map_err(|e| {
        warn!(
            target: "acp.protocol.spawn",
            session = %session_id,
            "runner spawn failed: {e}"
        );
        AcpError::Spawn(format!("spawn runner: {e}"))
    })?;
    let pid = child
        .id()
        .ok_or_else(|| AcpError::Spawn("runner exited before it could be identified".into()))?;
    // Reap it, so a finished runner does not linger as a zombie that still
    // answers `kill(pid, 0)` and keeps a teardown from proving it gone.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok((pid, native_store))
}
