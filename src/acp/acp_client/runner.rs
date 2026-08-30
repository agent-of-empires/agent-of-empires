//! Spawning the detached runner process and waiting for its control socket.

use tracing::{info, warn};

use super::errors::AcpError;
use super::resolve_command::resolve_agent_command;
use super::session_sandbox::{build_sandbox_docker_argv, SessionSandbox};
use super::spawn::{apply_env_filter, host_environment_denyreason, SpawnConfig};

/// Deadline for the runner unix socket to appear after spawning the
/// `aoe __acp-runner` shim. 10s is enough in production, but a
/// debug-build cold-start under heavy CI load (v8 coverage + multiple
/// parallel `aoe serve` binaries + a runner subprocess that re-execs
/// the same debug binary) can blow past it deterministically. Honors
/// `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS` in debug builds so the
/// Playwright harness can lift it; release builds keep the original
/// 10s ceiling.
pub(super) fn runner_socket_deadline() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            // Clamp to a floor of 100ms so a typo like
            // `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS=0` does not make
            // wait_for_socket fail immediately and surface as a
            // mysterious "runner socket did not appear" without ever
            // polling.
            return std::time::Duration::from_millis(ms.max(100));
        }
    }
    std::time::Duration::from_secs(10)
}

/// Test-only fault injection for the #1890 regression e2e. When
/// `AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES=N` is set, the first N *fresh*-spawn
/// ACP handshakes fail right after the runner has come up, before the daemon
/// records an in-memory worker. The runner keeps its agent alive and its
/// on-disk registry entry, so the daemon is left with a live, registered
/// runner it never adopted: the exact orphan state #1890 got permanently
/// stuck in, reproduced deterministically without depending on host timing.
/// Each call consumes one budgeted failure; `0` (the default, var unset) is a
/// no-op. Debug builds only, so release can never trip it. Mirrors the
/// `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS` debug knob above.
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

/// Spawn the `aoe __acp-runner` shim as a detached process. The
/// runner owns the agent subprocess and outlives the daemon. We retain
/// no `Child` handle here; once the runner is up, the daemon talks to
/// it over the unix socket and the OS keeps the runner alive across
/// `aoe serve` restarts.
pub(super) fn spawn_runner_detached(
    config: &SpawnConfig,
    socket_path: &std::path::Path,
    session_id: String,
    session_sandbox: Option<&SessionSandbox>,
) -> Result<(), AcpError> {
    use std::process::Command as StdCommand;
    let current_exe =
        std::env::current_exe().map_err(|e| AcpError::Spawn(format!("current_exe: {e}")))?;
    let log_path = crate::process::worker_registry::log_path_for(&session_id)
        .map_err(|e| AcpError::Spawn(format!("log path: {e}")))?;
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Sandboxed sessions wrap the agent in `docker exec`. Host-side
    // PATH resolution is skipped because the agent binary lives inside
    // the container; the container's own PATH resolves it. The
    // container_workdir is reused from the SessionSandbox built upstream
    // so we don't redo `compute_volume_paths`.
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

    // Resolve the agent binary against PATH + known node-manager dirs so
    // the runner spawns the right binary even when the daemon's frozen
    // PATH doesn't contain it. See #1048. The resolved bin dir is also
    // prepended to PATH below so the adapter's own `node`/`npx`
    // subprocesses land in the same install.
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

    let mut cmd = StdCommand::new(&current_exe);
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

    // Env: apply the same allowlist + provider_env filtering that the
    // legacy in-proc path does, then hand the cleaned env to the runner.
    // The runner inherits this env when it spawns the agent (no second
    // filter pass needed). AOE_TOKEN is stripped here so it never reaches
    // either process.
    cmd.env_clear();
    apply_env_filter(&mut cmd, config);
    // Trusted `Config.environment`, destined for the adapter only. It rides
    // one reserved carrier key rather than the runner's own environment
    // because HOME / PATH / XDG_CONFIG_HOME are legal entries here: setting
    // them on the runner would move the worker-registry path it writes (the
    // respawn loop of #1383) or change which binary it loads, whereas the
    // terminal-view equivalent only ever prefixes the agent's own command.
    // The runner strips the carrier and applies the decoded pairs to the
    // adapter child. Wire format: JSON `[[key, value], ...]`.
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
        // The agent runs inside the container; docker reads each
        // `-e KEY` flag's value from its own process env. Set the
        // corresponding values on the runner so docker (its child)
        // can forward them across the container boundary.
        for (key, value) in &s.inherit_env {
            cmd.env(key, value);
        }
    } else if let Some(dir) = &config.artifact_dir {
        // Non-sandboxed: the agent runs on the host, so point it directly at
        // the host artifact dir. The sandbox path exports the fixed container
        // mount as a `-e` flag in build_sandbox_docker_argv instead. See #2587.
        cmd.env(crate::session::artifacts::ARTIFACT_DIR_ENV, dir);
    }
    if !extra_path_dirs.is_empty() {
        // Prepend the resolved adapter bin dir (and, for a bundled adapter,
        // the Node bin dir) to the PATH we just forwarded so the adapter and
        // its `#!/usr/bin/env node` shim resolve against the same install,
        // not whatever node happens to be on the daemon's frozen PATH.
        let current = std::env::var_os("PATH").unwrap_or_default();
        let existing: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
        let mut chain: Vec<std::path::PathBuf> = Vec::new();
        for dir in &extra_path_dirs {
            if !existing.contains(dir) && !chain.contains(dir) {
                chain.push(dir.clone());
            }
        }
        chain.extend(existing);
        if let Ok(joined) = std::env::join_paths(&chain) {
            cmd.env("PATH", joined);
        }
    }

    // Detach: child becomes its own session leader so a SIGTERM/SIGHUP
    // to the aoe daemon's group doesn't cascade. The runner installs its
    // own signal handlers.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::other)?;
                Ok(())
            });
        }
    }

    // Redirect stdio: the runner writes its own log file. Inheriting our
    // stdio would (a) pollute the shared debug.log with the per-session
    // noise and (b) keep a pipe open to the daemon, which then closes
    // when we die, making the runner observe EOF on its own stdin/stdout.
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

    cmd.spawn().map_err(|e| {
        warn!(
            target: "acp.protocol.spawn",
            session = %session_id,
            "runner spawn failed: {e}"
        );
        AcpError::Spawn(format!("spawn runner: {e}"))
    })?;
    // Drop the std::process::Child here. std::process::Command doesn't
    // wait on drop, so the runner stays alive. setsid + nohup-equivalent
    // make this an actual detach.
    Ok(())
}

/// Poll the socket file's existence with `connect()` until a deadline.
/// Used by `connect_via_socket` to wait for the runner to finish binding
/// before the daemon dials in.
pub(super) async fn wait_for_socket(
    path: &std::path::Path,
    deadline: std::time::Duration,
) -> Result<tokio::net::UnixStream, AcpError> {
    let started = std::time::Instant::now();
    let mut delay_ms = 20_u64;
    loop {
        if path.exists() {
            match tokio::net::UnixStream::connect(path).await {
                Ok(s) => return Ok(s),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused) => {
                    // Listener not yet ready; back off and retry.
                }
                Err(e) => return Err(AcpError::Spawn(format!("connect {}: {e}", path.display()))),
            }
        }
        if started.elapsed() >= deadline {
            return Err(AcpError::Spawn(format!(
                "runner socket {} did not appear within {}s",
                path.display(),
                deadline.as_secs()
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        delay_ms = (delay_ms * 2).min(200);
    }
}
