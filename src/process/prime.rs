//! Prime's detached workers require a launch-owned daemon and socket namespace.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::Status;

// Killing the local container CLI does not terminate its remote exec process.
const CONTAINER_FRONTEND_FENCE: &str = r#"marker=$1
socket=$2
identity_alive() {
    local pid=$1 expected=$2 stat fields
    [[ -d /proc/$pid ]] || return 1
    IFS= read -r stat < /proc/$pid/stat || { [[ ! -d /proc/$pid ]] && return 1; return 2; }
    read -r -a fields <<< "${stat##*) }"
    [[ ${fields[0]} != Z ]] || return 1
    [[ -n ${fields[19]} ]] || return 2
    [[ -z $expected || ${fields[19]} == "$expected" ]] || return 1
    matched_birth=${fields[19]}
}
match_frontend() {
    local pid=$1 expected=$2 daemon=0 route=0 argv i code
    identity_alive "$pid" "$expected" || return $?
    mapfile -d '' -t argv < /proc/$pid/cmdline 2>/dev/null || { [[ ! -d /proc/$pid ]] && return 1; return 2; }
    for ((i=0; i+1<${#argv[@]}; i++)); do
        [[ ${argv[i]} == --mode && ${argv[i+1]} == daemon ]] && daemon=1
        [[ ${argv[i]} == --daemon-socket && ${argv[i+1]} == "$socket" ]] && route=1
    done
    [[ $route == 1 && $daemon == 0 ]] || return 1
    grep -z -F -x -- "$marker" /proc/$pid/environ >/dev/null 2>&1; code=$?
    if [[ $code > 1 ]]; then [[ ! -d /proc/$pid ]] && return 1; return 2; fi
    [[ $code == 0 ]] || return 1
    identity_alive "$pid" "$matched_birth"
}
scan() {
    local directory pid code
    for directory in /proc/[0-9]*; do
        pid=${directory#/proc/}
        match_frontend "$pid" ""; code=$?
        [[ $code != 2 ]] || return 1
        [[ $code != 0 ]] || printf '%s %s\n' "$pid" "$matched_birth"
    done
}
selected=$(scan) || exit 1
[[ -n $selected ]] || { printf settled; exit 0; }
for signal in TERM KILL; do
    while read -r pid birth; do
        identity_alive "$pid" "$birth"; code=$?
        [[ $code != 2 ]] || exit 1
        [[ $code == 0 ]] || continue
        match_frontend "$pid" "$birth"; code=$?
        if [[ $code != 0 ]]; then
            identity_alive "$pid" "$birth"; [[ $? == 1 ]] || exit 1
            continue
        fi
        if ! kill -"$signal" "$pid"; then
            identity_alive "$pid" "$birth"; [[ $? == 1 ]] || exit 1
        fi
    done <<< "$selected"
    for ((attempt=0; attempt<10; attempt++)); do
        active=0
        while read -r pid birth; do
            identity_alive "$pid" "$birth"; code=$?
            [[ $code != 2 ]] || exit 1
            [[ $code != 0 ]] || active=1
        done <<< "$selected"
        if [[ $active == 0 ]]; then
            selected=$(scan) || exit 1
            [[ -n $selected ]] || { printf settled; exit 0; }
            break
        fi
        sleep 0.05
    done
done
exit 1"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimeContainer {
    pub(crate) transport: crate::containers::RuntimeExecutionSnapshot,
    pub id: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimeRuntime {
    pub directory: PathBuf,
    pub program: PathBuf,
    pub agent_dir: PathBuf,
    pub container: Option<PrimeContainer>,
    #[serde(skip)]
    observation: Arc<Mutex<PrimeRuntimeState>>,
}

#[derive(Debug, Default)]
struct PrimeRuntimeState {
    observation: Option<(Instant, Option<Arc<PrimeObservation>>)>,
    cleaned: bool,
}

impl PartialEq for PrimeRuntime {
    fn eq(&self, other: &Self) -> bool {
        self.directory == other.directory
            && self.program == other.program
            && self.agent_dir == other.agent_dir
            && self.container == other.container
    }
}
impl Eq for PrimeRuntime {}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrimeObservation {
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub cwd: String,
    active_session_id: String,
    rlm_depth: u64,
    attached_clients: u64,
    worker_state: String,
    lifecycle: String,
    activity: String,
    is_streaming: bool,
    is_compacting: bool,
    has_running_subagents: bool,
    is_bash_running: bool,
    is_session_active: bool,
}

impl PrimeObservation {
    pub(crate) fn status(&self) -> Status {
        if self.activity == "working"
            || self.is_streaming
            || self.is_compacting
            || self.has_running_subagents
            || self.is_bash_running
            || self.is_session_active
        {
            Status::Running
        } else if self.activity == "idle" {
            Status::Idle
        } else {
            Status::Unknown
        }
    }
}

pub(crate) fn foreground_observation(bytes: &[u8]) -> Option<PrimeObservation> {
    let mut data: Value = serde_json::from_slice(bytes).ok()?;
    let rows = data.get_mut("sessions")?.as_array_mut()?;
    let mut roots = rows.iter_mut().filter(|row| {
        row.get("rlmDepth").and_then(Value::as_u64) == Some(0)
            && row
                .get("attachedClients")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0)
    });
    let root: PrimeObservation = serde_json::from_value(std::mem::take(roots.next()?)).ok()?;
    if roots.next().is_some()
        || root.rlm_depth != 0
        || root.attached_clients == 0
        || root.worker_state != "ready"
        || root.lifecycle == "recovering"
        || root.active_session_id.is_empty()
        || root.cwd.is_empty()
        || !crate::session::capture::is_valid_session_id(&root.session_id)
    {
        return None;
    }
    Some(root)
}

impl PrimeRuntime {
    pub fn new(program: PathBuf, agent_dir: PathBuf, container: Option<PrimeContainer>) -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        Self {
            directory: PathBuf::from(format!("/tmp/aoe-prime-{}", &id[..12])),
            program,
            agent_dir,
            container,
            observation: Arc::default(),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.directory.join("cleanup/daemon.sock")
    }

    fn validate(&self) -> Result<()> {
        let name = self
            .directory
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        anyhow::ensure!(
            self.directory.parent() == Some(Path::new("/tmp"))
                && name.strip_prefix("aoe-prime-").is_some_and(|id| {
                    id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit())
                })
                && self.program.is_absolute()
                && self.agent_dir.is_absolute(),
            "invalid Prime runtime route"
        );
        Ok(())
    }

    fn command(&self, cleanup: bool) -> Command {
        let root = self.directory.join("cleanup");
        let mut command = match &self.container {
            Some(container) => {
                let mut cmd = container.transport.command(&[]);
                cmd.args(["exec", "-w"])
                    .arg(&self.directory)
                    .arg(&container.id)
                    .arg("env")
                    .arg(format!("TMPDIR={}", self.directory.display()));
                if cleanup {
                    cmd.arg(format!("PRIME_AGENT_CODING_AGENT_DIR={}", root.display()))
                        .arg(format!("HOME={}", self.directory.display()));
                }
                cmd.arg(&self.program);
                cmd
            }
            None => {
                let mut cmd = Command::new(&self.program);
                cmd.env("TMPDIR", &self.directory);
                if cleanup {
                    cmd.env("PRIME_AGENT_CODING_AGENT_DIR", root)
                        .env("HOME", &self.directory);
                }
                cmd.current_dir(&self.directory);
                cmd
            }
        };
        command.stdin(Stdio::null());
        command
    }

    pub(crate) fn prepare(&self) -> Result<()> {
        self.validate()?;
        let cleanup = self.directory.join("cleanup");
        let workers = self.agent_dir.join("daemon-workers");
        if let Some(container) = &self.container {
            let script = format!(
                "umask 077; mkdir {} && mkdir {} && ln -s {} {}",
                crate::session::environment::shell_escape(&self.directory.to_string_lossy()),
                crate::session::environment::shell_escape(&cleanup.to_string_lossy()),
                crate::session::environment::shell_escape(&workers.to_string_lossy()),
                crate::session::environment::shell_escape(
                    &cleanup.join("daemon-workers").to_string_lossy()
                ),
            );
            let mut cmd = container.transport.command(&[]);
            cmd.args(["exec", &container.id, "sh", "-c", &script]);
            crate::session::capture::run_with_timeout_limit(
                cmd,
                Duration::from_secs(5),
                "Prime runtime setup",
                8192,
            )?;
        } else {
            use std::os::unix::fs::{symlink, DirBuilderExt};
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(&self.directory)?;
            builder.create(&cleanup)?;
            symlink(workers, cleanup.join("daemon-workers"))?;
        }
        Ok(())
    }

    pub(crate) fn observe(&self, fresh: bool) -> Option<Arc<PrimeObservation>> {
        self.validate().ok()?;
        let mut cache = self.observation.lock().ok()?;
        if cache.cleaned {
            return None;
        }
        if !fresh {
            if let Some((at, observation)) = &cache.observation {
                if at.elapsed() < Duration::from_millis(500) {
                    return observation.clone();
                }
            }
        }
        let mut command = self.command(false);
        command
            .args(["list", "--json", "--socket"])
            .arg(self.socket());
        let observation = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(1),
            "Prime session observation",
            256 * 1024,
        )
        .ok()
        .and_then(|bytes| foreground_observation(&bytes))
        .map(Arc::new);
        cache.observation = Some((Instant::now(), observation.clone()));
        observation
    }

    fn has_remaining_processes(&self) -> Result<bool> {
        let needle = format!("TMPDIR={}", self.directory.display());
        if let Some(container) = &self.container {
            let script = format!(
                r#"for file in /proc/[0-9]*/environ; do [ -r "$file" ] || continue; if tr '\0' '\n' < "$file" 2>/dev/null | grep -Fxq -- {}; then printf present; exit 0; fi; done; printf absent"#,
                crate::session::environment::shell_escape(&needle)
            );
            let mut probe = container.transport.command(&[]);
            probe.args(["exec", &container.id, "sh", "-c", &script]);
            let output = crate::session::capture::run_with_timeout_limit(
                probe,
                Duration::from_secs(2),
                "Prime process quiescence",
                1024,
            )?;
            return match output.as_slice() {
                b"present" => Ok(true),
                b"absent" => Ok(false),
                _ => anyhow::bail!("invalid Prime process quiescence result"),
            };
        }
        Ok(crate::process::processes_matching(&[needle], &[None], &[None])[0])
    }

    fn fence_container_frontends(&self) -> Result<()> {
        let Some(container) = &self.container else {
            return Ok(());
        };
        let mut command = container.transport.command(&[]);
        command.args([
            "exec",
            &container.id,
            "bash",
            "-c",
            CONTAINER_FRONTEND_FENCE,
            "sh",
        ]);
        command
            .arg(format!("TMPDIR={}", self.directory.display()))
            .arg(self.socket());
        let result = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(5),
            "Prime container frontend fence",
            1024,
        )?;
        anyhow::ensure!(
            result == b"settled",
            "Prime container frontend termination unproven"
        );
        Ok(())
    }

    pub fn cleanup(&self) -> Result<()> {
        self.validate()?;
        let mut state = self
            .observation
            .lock()
            .map_err(|_| anyhow::anyhow!("Prime observation lock poisoned"))?;
        if state.cleaned {
            return Ok(());
        }
        if self.container.is_none() && !self.directory.exists() {
            return Ok(());
        }
        self.fence_container_frontends()?;
        // Cleanup is confined to this launch even though worker metadata is shared.
        let mut command = self.command(true);
        command.args(["shutdown", "--force", "--json"]);
        let bytes = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(20),
            "Prime runtime shutdown",
            256 * 1024,
        )?;
        let response: Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            response
                .get("failed")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
            "Prime runtime shutdown did not prove termination"
        );
        state.observation = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.has_remaining_processes()? {
            anyhow::ensure!(
                Instant::now() < deadline,
                "Prime-owned processes remain after native shutdown"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        state.cleaned = true;
        Ok(())
    }
}

pub(crate) struct PrimeCleanupGuard(
    pub Option<PrimeRuntime>,
    pub Option<Arc<tokio::sync::Mutex<tokio::process::Child>>>,
);
impl Drop for PrimeCleanupGuard {
    fn drop(&mut self) {
        if let Some(child) = &self.1 {
            if let Ok(mut child) = child.try_lock() {
                if let Some(pid) = child.id() {
                    crate::process::kill_process_tree(pid);
                }
                child.start_kill().ok();
            }
        }
        if let Some(runtime) = &self.0 {
            if let Err(error) = runtime.cleanup() {
                tracing::warn!(target: "process.prime", "Prime cleanup failed: {error}");
            }
        }
    }
}

#[derive(Debug, Args)]
pub struct PrimeRunnerArgs {
    #[arg(long)]
    pub runtime: String,
    #[arg(long)]
    pub daemon_command: Option<String>,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub async fn run(args: PrimeRunnerArgs) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let runtime: PrimeRuntime = serde_json::from_str(&args.runtime)?;
    runtime.prepare()?;
    let mut guard = PrimeCleanupGuard(Some(runtime.clone()), None);
    let mut daemon = if let Some(command) = &args.daemon_command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    } else if let Some(container) = &runtime.container {
        let index = args
            .command
            .iter()
            .position(|word| word == &container.id)
            .context("Prime frontend has no pinned container")?;
        let mut cmd = tokio::process::Command::from(container.transport.command(&[]));
        cmd.args(&args.command[1..=index]);
        cmd.arg("env")
            .arg(format!("TMPDIR={}", runtime.directory.display()));
        cmd.arg(&runtime.program)
            .args(["--mode", "daemon", "--daemon-socket"])
            .arg(runtime.socket());
        cmd
    } else {
        let mut cmd = tokio::process::Command::new(&runtime.program);
        cmd.args(["--mode", "daemon", "--daemon-socket"])
            .arg(runtime.socket());
        cmd
    };
    daemon
        .env("TMPDIR", &runtime.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    daemon.process_group(0);
    let mut daemon = daemon.spawn().context("starting owned Prime daemon")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if daemon.try_wait()?.is_some() {
            anyhow::bail!("Prime daemon exited during startup");
        }
        let mut probe = runtime.command(false);
        probe
            .args(["list", "--json", "--socket"])
            .arg(runtime.socket());
        let ready = tokio::task::spawn_blocking(move || {
            crate::session::capture::run_with_timeout_limit(
                probe,
                Duration::from_millis(500),
                "Prime daemon readiness",
                256 * 1024,
            )
            .is_ok()
        })
        .await?;
        if ready {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "Prime daemon startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut argv = args.command;
    if let Some(container) = &runtime.container {
        let index = argv.iter().position(|word| word == &container.id);
        if let Some(index) = index {
            argv.splice(
                index..index,
                [
                    "--env".into(),
                    format!("TMPDIR={}", runtime.directory.display()),
                ],
            );
        }
    }
    let mut frontend = match &runtime.container {
        Some(container) if argv.iter().any(|arg| arg == &container.id) => {
            tokio::process::Command::from(container.transport.command(&[]))
        }
        _ => tokio::process::Command::new(&argv[0]),
    };
    frontend
        .args(&argv[1..])
        .env("TMPDIR", &runtime.directory)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut frontend = frontend.spawn().context("starting Prime frontend")?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let status = tokio::select! {
        status = frontend.wait() => Some(status?),
        _ = daemon.wait() => None,
        _ = terminate.recv() => None,
        _ = hangup.recv() => None,
    };
    frontend.start_kill().ok();
    frontend.wait().await.ok();
    let owned = runtime.clone();
    tokio::task::spawn_blocking(move || owned.cleanup()).await??;

    daemon.start_kill().ok();
    daemon.wait().await.ok();
    guard.0.take();
    if let Some(status) = status {
        anyhow::ensure!(status.success(), "Prime frontend exited with {status}");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreground_root_and_activity_are_authoritative() {
        let row = serde_json::json!({"sessionId":"durable-root", "sessionFile":null,"cwd":"/workspace", "activeSessionId":"daemon-address", "rlmDepth":0,"attachedClients":1,"workerState":"ready","lifecycle":"active","activity":"idle","isStreaming":false,"isCompacting":false,"hasRunningSubagents":false,"isBashRunning":false,"isSessionActive":false});
        for (field, value, expected) in [
            (
                "activity",
                serde_json::json!("working"),
                Some(Status::Running),
            ),
            (
                "isBashRunning",
                serde_json::json!(true),
                Some(Status::Running),
            ),
            (
                "isSessionActive",
                serde_json::json!(true),
                Some(Status::Running),
            ),
            (
                "activity",
                serde_json::json!("unavailable"),
                Some(Status::Unknown),
            ),
            ("rlmDepth", serde_json::json!(1), None),
            ("attachedClients", serde_json::json!(0), None),
            ("lifecycle", serde_json::json!("recovering"), None),
            ("workerState", serde_json::json!("spawning"), None),
        ] {
            let mut changed = row.clone();
            changed[field] = value;
            let bytes = serde_json::to_vec(&serde_json::json!({"sessions":[changed]})).unwrap();
            assert_eq!(
                foreground_observation(&bytes).map(|root| root.status()),
                expected,
                "{field}"
            );
        }
        let mut peer = row.clone();
        peer["sessionId"] = serde_json::json!("peer-root");
        peer["activeSessionId"] = serde_json::json!("peer-address");
        let bytes = serde_json::to_vec(&serde_json::json!({"sessions":[row,peer]})).unwrap();
        assert!(foreground_observation(&bytes).is_none());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn remote_frontend_fence_preserves_daemons_and_foreign_routes() {
        use std::io::BufRead as _;
        struct Waiter(std::process::Child);
        impl Drop for Waiter {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let marker = format!("TMPDIR={}", root.path().display());
        let socket = root
            .path()
            .join("daemon.sock")
            .to_string_lossy()
            .into_owned();
        let foreign = root.path().join("foreign");
        let spawn = |tmp: &Path, args: &[&str], embedded: bool| {
            let mut command = Command::new("bash");
            command
                .args([
                    "-c",
                    "trap '' TERM; printf 'ready\n'; while IFS= read -r line; do :; done",
                    "fixture",
                ])
                .args(args)
                .env("TMPDIR", tmp)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if embedded {
                command.env("OTHER", format!("not-a-marker\n{marker}"));
            }
            let mut child = Waiter(command.spawn().unwrap());
            let mut ready = String::new();
            std::io::BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            child
        };
        let mut owned = spawn(
            root.path(),
            &["--mode", "acp", "--daemon-socket", &socket],
            false,
        );
        let mut protected = [
            spawn(
                root.path(),
                &["--mode", "daemon", "--daemon-socket", &socket],
                false,
            ),
            spawn(&foreign, &["--daemon-socket", &socket], false),
            spawn(root.path(), &["--daemon-socket", "another-socket"], false),
            spawn(&foreign, &["--daemon-socket", &socket], true),
            spawn(root.path(), &[&format!("--daemon-socket {socket}")], false),
        ];
        let mut command = Command::new("bash");
        command.args(["-c", CONTAINER_FRONTEND_FENCE, "sh", &marker, &socket]);
        let output = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(5),
            "frontend fence regression",
            1024,
        )
        .unwrap();
        assert_eq!(output, b"settled");
        assert!(!owned.0.wait().unwrap().success());
        for waiter in &mut protected {
            assert!(waiter.0.try_wait().unwrap().is_none());
        }
    }
}
