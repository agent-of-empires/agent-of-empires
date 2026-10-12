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

fn sha256_prefix(input: &[u8], bytes: usize) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(input);
    let mut key = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        write!(key, "{byte:02x}").unwrap();
    }
    key
}

const RECORDED_LAUNCH: &str = r#"identity=$1
shift
if [[ -r /proc/$$/stat ]]; then
    IFS= read -r stat < /proc/$$/stat || exit 1
    read -r -a fields <<< "${stat##*) }"
    [[ -n ${fields[19]} ]] || exit 1
    start="proc:${fields[19]}"
else
    start=$(LC_ALL=C LC_TIME=C LANG=C TZ=UTC ps -p "$$" -o lstart=) || exit 1
    start="${start#"${start%%[![:space:]]*}"}"
    [[ -n $start ]] || exit 1
    start="ps:$start"
fi
(umask 077; printf '%s\t%s\n' "$$" "$start" > "$identity") || exit 1
exec "$@""#;

// The recorded launch incarnation is checked again before each guest signal.
const CONTAINER_PROCESS_FENCE: &str = r#"pid=$1
expected=${2#proc:}
identity_alive() {
    local stat fields
    [[ -d /proc/$pid ]] || return 1
    IFS= read -r stat < /proc/$pid/stat || { [[ ! -d /proc/$pid ]] && return 1; return 2; }
    read -r -a fields <<< "${stat##*) }"
    [[ ${fields[0]} != Z ]] || return 1
    [[ -n ${fields[19]} ]] || return 2
    [[ ${fields[19]} == "$expected" ]] || return 1
}
for signal in TERM KILL; do
    identity_alive; code=$?
    [[ $code != 2 ]] || exit 1
    [[ $code != 1 ]] || { printf settled; exit 0; }
    if ! kill -"$signal" "$pid"; then
        identity_alive; [[ $? == 1 ]] || exit 1
    fi
    for ((attempt=0; attempt<10; attempt++)); do
        identity_alive; code=$?
        [[ $code != 2 ]] || exit 1
        [[ $code != 1 ]] || { printf settled; exit 0; }
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

#[derive(Serialize, Deserialize)]
struct PrimeCleanupProof {
    route: String,
    processes: Vec<crate::process::PrimeProcessIdentity>,
    settled: bool,
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
        let _lock = self.lock_proof()?;
        self.write_proof(&PrimeCleanupProof {
            route: self.proof_route()?,
            processes: Vec::new(),
            settled: false,
        })?;
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

    fn proof_paths(&self) -> Result<(PathBuf, PathBuf)> {
        let directory = crate::process::worker_registry::workers_dir()?;
        let name = self
            .directory
            .file_name()
            .context("missing Prime runtime name")?
            .to_string_lossy();
        Ok((
            directory.join(format!(".{name}.prime-lock")),
            directory.join(format!(".{name}.prime-proof")),
        ))
    }

    fn lock_proof(&self) -> Result<std::fs::File> {
        use fs2::FileExt as _;
        let (path, _) = self.proof_paths()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn proof_route(&self) -> Result<String> {
        Ok(sha256_prefix(&serde_json::to_vec(self)?, 32))
    }

    fn read_proof(&self) -> Result<PrimeCleanupProof> {
        let (_, path) = self.proof_paths()?;
        let proof: PrimeCleanupProof = serde_json::from_slice(
            &std::fs::read(path).context("Prime cleanup ownership ledger unavailable")?,
        )?;
        anyhow::ensure!(
            proof.route == self.proof_route()?,
            "Prime cleanup ledger belongs to another execution route"
        );
        Ok(proof)
    }

    fn write_proof(&self, proof: &PrimeCleanupProof) -> Result<()> {
        let (_, path) = self.proof_paths()?;
        crate::session::atomic_write(&path, &serde_json::to_vec(proof)?)
    }

    fn worker_descriptor_dir(&self) -> PathBuf {
        self.agent_dir.join("daemon-workers").join(sha256_prefix(
            self.socket().as_os_str().as_encoded_bytes(),
            6,
        ))
    }

    fn worker_identities(&self) -> Result<Vec<crate::process::PrimeProcessIdentity>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Descriptor {
            pid: u32,
            process_start_id: Option<String>,
            supervisor_socket_path: PathBuf,
        }
        let mut identities = Vec::new();
        let mut accept = |reader: &mut dyn std::io::Read| -> Result<()> {
            let descriptor: Descriptor = serde_json::from_reader(reader)?;
            anyhow::ensure!(
                descriptor.supervisor_socket_path == self.socket(),
                "Prime descriptor escaped its supervisor namespace"
            );
            if descriptor.pid != 0 {
                anyhow::ensure!(
                    descriptor.pid <= i32::MAX as u32,
                    "invalid native Prime worker PID"
                );
                let start_id = descriptor
                    .process_start_id
                    .filter(|value| !value.is_empty())
                    .context("native Prime worker has no process start identity")?;
                identities.push(crate::process::PrimeProcessIdentity {
                    pid: descriptor.pid,
                    start_id,
                });
            }
            Ok(())
        };
        let directory = self.worker_descriptor_dir();
        if let Some(container) = &self.container {
            let mut probe = container.transport.command(&[]);
            probe.args(["exec", &container.id, "sh", "-c", r#"if [ -L "$1" ]; then exit 1; elif [ -d "$1" ]; then printf present; elif [ ! -e "$1" ]; then printf absent; else exit 1; fi"#, "sh"]).arg(&directory);
            let presence = crate::session::capture::run_with_timeout_limit(
                probe,
                Duration::from_secs(2),
                "Prime worker descriptor namespace",
                1024,
            )?;
            if presence == b"absent" {
                return Ok(identities);
            }
            anyhow::ensure!(
                presence == b"present",
                "invalid Prime descriptor presence result"
            );
            let mut command = container.transport.command(&[]);
            command
                .arg("cp")
                .arg(format!("{}:{}/.", container.id, directory.display()))
                .arg("-");
            let bytes = crate::session::capture::run_with_timeout_limit(
                command,
                Duration::from_secs(5),
                "Prime worker descriptor snapshot",
                1024 * 1024,
            )?;
            let mut archive = tar::Archive::new(bytes.as_slice());
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?;
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    anyhow::ensure!(
                        entry.header().entry_type().is_file(),
                        "invalid Prime worker descriptor type"
                    );
                    accept(&mut entry)?;
                }
            }
        } else {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(identities)
                }
                Err(error) => {
                    return Err(error).context("reading Prime worker descriptor namespace")
                }
            };
            for entry in entries {
                let entry = entry?;
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    anyhow::ensure!(
                        entry.file_type()?.is_file(),
                        "invalid Prime worker descriptor type"
                    );
                    accept(&mut std::fs::File::open(entry.path())?)?;
                }
            }
        }
        Ok(identities)
    }

    fn process_snapshot(
        &self,
        known: &[crate::process::PrimeProcessIdentity],
    ) -> Result<Vec<crate::process::PrimeProcessIdentity>> {
        let marker = format!("TMPDIR={}", self.directory.display());
        if let Some(container) = &self.container {
            let script = r#"marker=$1
socket=$2
known=$3
seen=$'\n'
[[ -r /proc/self/stat ]] || exit 1
for directory in /proc/[0-9]*; do
    pid=${directory#/proc/}
    [[ $pid != $BASHPID ]] || continue
    if ! IFS= read -r stat < "$directory/stat"; then
        [[ ! -d $directory ]] && continue
        [[ $known != *$'\n'"$pid"$'\t'* ]] || exit 1
        continue
    fi
    read -r -a fields <<< "${stat##*) }"
    [[ ${fields[0]} != Z ]] || continue
    [[ -n ${fields[19]} && -n ${fields[1]} ]] || exit 1
    marked=0
    grep -z -F -x -- "$marker" "$directory/environ" >/dev/null 2>&1 && marked=1
    grep -z -F -x -- "$socket" "$directory/cmdline" >/dev/null 2>&1 && marked=1
    printf '%s\tproc:%s\t%s\t%s\n' "$pid" "${fields[19]}" "${fields[1]}" "$marked"
    seen+="$pid"$'\t'"proc:${fields[19]}"$'\n'
done
while IFS=$'\t' read -r pid start; do
    [[ -z $pid || ! -d /proc/$pid ]] && continue
    IFS= read -r stat < /proc/$pid/stat || { [[ ! -d /proc/$pid ]] && continue; exit 1; }
    read -r -a fields <<< "${stat##*) }"
    [[ ${fields[0]} == Z || "proc:${fields[19]}" != "$start" ]] && continue
    [[ $seen == *$'\n'"$pid"$'\t'"$start"$'\n'* ]] || exit 1
done <<< "$known""#;
            let known_wire = known
                .iter()
                .fold(String::from("\n"), |mut value, identity| {
                    use std::fmt::Write as _;
                    writeln!(value, "{}\t{}", identity.pid, identity.start_id).unwrap();
                    value
                });
            let mut command = container.transport.command(&[]);
            command
                .args(["exec", &container.id, "bash", "-c", script, "sh", &marker])
                .arg(self.socket())
                .arg(known_wire);
            let bytes = crate::session::capture::run_with_timeout_limit(
                command,
                Duration::from_secs(2),
                "Prime scoped process snapshot",
                1024 * 1024,
            )?;
            let mut samples = Vec::new();
            for line in std::str::from_utf8(&bytes)?.lines() {
                let mut fields = line.split('\t');
                let pid = fields.next().context("missing process PID")?.parse()?;
                let start_id = fields
                    .next()
                    .context("missing process identity")?
                    .to_owned();
                let parent = fields.next().context("missing process parent")?.parse()?;
                let marked = match fields.next() {
                    Some("1") => true,
                    Some("0") => false,
                    _ => anyhow::bail!("invalid process launch marker"),
                };
                anyhow::ensure!(fields.next().is_none(), "invalid process snapshot row");
                samples.push(crate::process::PrimeProcessSample {
                    identity: crate::process::PrimeProcessIdentity { pid, start_id },
                    parent,
                    marked,
                });
            }
            return Ok(crate::process::select_prime_processes(samples, known));
        }
        crate::process::prime_processes(&marker, &self.socket(), known)
    }

    fn launch_identity_path(&self, daemon: bool) -> PathBuf {
        self.directory.join(if daemon {
            "daemon.identity"
        } else {
            "frontend.identity"
        })
    }

    pub(crate) fn recorded_launch_command(&self, command: &str, daemon: bool) -> String {
        use crate::session::environment::shell_escape_script_word as escape;
        format!(
            "bash -c {} sh {} env {command}",
            escape(RECORDED_LAUNCH),
            escape(&self.launch_identity_path(daemon).to_string_lossy())
        )
    }

    fn launch_identity(
        &self,
        daemon: bool,
    ) -> Result<Option<crate::process::PrimeProcessIdentity>> {
        let path = self.launch_identity_path(daemon);
        let text = if let Some(container) = &self.container {
            let mut command = container.transport.command(&[]);
            command.args(["exec", &container.id, "sh", "-c", "if [ -L \"$1\" ]; then exit 1; elif [ -f \"$1\" ]; then cat -- \"$1\"; elif [ -e \"$1\" ]; then exit 1; else printf absent; fi", "sh"]).arg(&path);
            let bytes = crate::session::capture::run_with_timeout_limit(
                command,
                Duration::from_secs(5),
                "Prime launch identity",
                1024,
            )?;
            String::from_utf8(bytes)?
        } else {
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("inspecting Prime launch identity"),
                Ok(metadata) => anyhow::ensure!(
                    metadata.file_type().is_file(),
                    "invalid Prime launch identity file"
                ),
            }
            std::fs::read_to_string(&path)?
        };
        if text == "absent" {
            return Ok(None);
        }
        let (pid, start) = text
            .trim_end_matches('\n')
            .split_once('\t')
            .context("invalid Prime launch identity")?;
        let pid: u32 = pid.parse()?;
        anyhow::ensure!(
            pid > 0
                && pid <= i32::MAX as u32
                && (start
                    .strip_prefix("proc:")
                    .is_some_and(|ticks| ticks.parse::<u64>().is_ok())
                    || (self.container.is_none()
                        && start
                            .strip_prefix("ps:")
                            .is_some_and(|birth| !birth.is_empty()))),
            "invalid Prime launch incarnation"
        );
        Ok(Some(crate::process::PrimeProcessIdentity {
            pid,
            start_id: start.into(),
        }))
    }

    fn fence_container_processes(&self, daemon: bool) -> Result<()> {
        let Some(container) = &self.container else {
            return Ok(());
        };
        let Some(identity) = self.launch_identity(daemon)? else {
            return Ok(());
        };
        let mut command = container.transport.command(&[]);
        command
            .args([
                "exec",
                &container.id,
                "bash",
                "-c",
                CONTAINER_PROCESS_FENCE,
                "sh",
            ])
            .arg(identity.pid.to_string())
            .arg(&identity.start_id);
        let result = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(5),
            "Prime container launch fence",
            1024,
        )?;
        anyhow::ensure!(
            result == b"settled",
            "Prime container launch termination unproven"
        );
        Ok(())
    }

    fn stop_owned_daemon(&self) -> Result<()> {
        if self.container.is_some() {
            return self.fence_container_processes(true);
        }
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        if let Some(stream) = crate::process::worker::connect_with_timeout(&self.socket()) {
            use std::io::{BufRead as _, Read as _, Write as _};
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_millis(500)))?;
            let mut reader = std::io::BufReader::new(stream.take(256 * 1024));
            let mut frame = String::new();
            reader.read_line(&mut frame)?;
            let hello: Value = serde_json::from_str(&frame)?;
            anyhow::ensure!(
                hello.get("type").and_then(Value::as_str) == Some("daemon_hello")
                    && hello.pointer("/protocol/name").and_then(Value::as_str)
                        == Some("prime-agent.daemon")
                    && hello.pointer("/protocol/version").and_then(Value::as_u64) == Some(7)
                    && hello.get("socketPath").and_then(Value::as_str) == self.socket().to_str(),
                "unsupported Prime daemon shutdown protocol"
            );
            reader.get_mut().get_mut().write_all(b"{\"type\":\"command\",\"id\":\"aoe-cleanup\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"clientId\":\"aoe\",\"command\":{\"type\":\"shutdown\",\"id\":\"aoe-cleanup\",\"force\":true}}\n")?;
            loop {
                frame.clear();
                anyhow::ensure!(
                    reader.read_line(&mut frame)? != 0,
                    "Prime daemon closed without accepting shutdown"
                );
                let response: Value = serde_json::from_str(&frame)?;
                if response.get("type").and_then(Value::as_str) == Some("response")
                    && response.get("id").and_then(Value::as_str) == Some("aoe-cleanup")
                {
                    anyhow::ensure!(
                        response.get("success").and_then(Value::as_bool) == Some(true),
                        "Prime daemon rejected shutdown"
                    );
                    break;
                }
            }
        }
        Ok(())
    }

    fn directory_exists(&self, settled: bool) -> Result<bool> {
        if let Some(container) = &self.container {
            let mut command = container.transport.command(&[]);
            command.args(["exec", &container.id, "sh", "-c",
                r#"if [ -L "$1" ]; then exit 1; elif [ -d "$1" ]; then if [ "$2" = 0 ]; then [ ! -L "$1/cleanup" ] && [ -d "$1/cleanup" ] || exit 1; fi; printf present; elif [ ! -e "$1" ]; then printf absent; else exit 1; fi"#, "sh"])
                .arg(&self.directory).arg(if settled { "1" } else { "0" });
            let bytes = crate::session::capture::run_with_timeout_limit(
                command,
                Duration::from_secs(2),
                "Prime runtime presence",
                1024,
            )?;
            return match bytes.as_slice() {
                b"present" => Ok(true),
                b"absent" => Ok(false),
                _ => anyhow::bail!("invalid Prime runtime presence result"),
            };
        }
        match std::fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                anyhow::ensure!(metadata.is_dir(), "invalid Prime runtime directory");
                if !settled {
                    anyhow::ensure!(
                        std::fs::symlink_metadata(self.directory.join("cleanup"))?.is_dir(),
                        "invalid Prime cleanup namespace"
                    );
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("checking Prime runtime directory"),
        }
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
        let _lock = self.lock_proof()?;
        let mut proof = self.read_proof()?;
        let exists = self.directory_exists(proof.settled)?;
        if !proof.settled {
            anyhow::ensure!(
                exists,
                "Prime runtime disappeared without a termination receipt"
            );
            proof.processes.extend(self.worker_identities()?);
            proof.processes.extend(self.launch_identity(true)?);
            proof.processes.extend(self.launch_identity(false)?);
            proof.processes = self.process_snapshot(&proof.processes)?;
            // Native shutdown deletes worker descriptors before every auxiliary process has exited.
            self.write_proof(&proof)?;
            self.fence_container_processes(false)?;
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
            self.stop_owned_daemon()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let remaining = self.process_snapshot(&proof.processes)?;
                if remaining != proof.processes {
                    proof.processes = remaining;
                    self.write_proof(&proof)?;
                }
                if proof.processes.is_empty() {
                    break;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Prime-owned processes remain after native shutdown; retained identity ledger"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            proof.settled = true;
            self.write_proof(&proof)?;
        }
        let remaining = self.process_snapshot(&proof.processes)?;
        if !remaining.is_empty() {
            proof.processes = remaining;
            proof.settled = false;
            self.write_proof(&proof)?;
            anyhow::bail!("Prime processes outlived their termination receipt");
        }
        if exists {
            if let Some(container) = &self.container {
                let mut command = container.transport.command(&[]);
                command
                    .args(["exec", &container.id, "rm", "-rf", "--"])
                    .arg(&self.directory);
                crate::session::capture::run_with_timeout_limit(
                    command,
                    Duration::from_secs(5),
                    "Prime runtime disposal",
                    8192,
                )?;
            } else {
                match std::fs::remove_dir_all(&self.directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("removing settled Prime runtime"),
                }
            }
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
        if let Some(runtime) = self.0.take() {
            let cleanup = move || {
                if let Err(error) = runtime.cleanup() {
                    tracing::warn!(target: "process.prime", "Prime cleanup failed: {error}");
                }
            };
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(cleanup);
            } else {
                cleanup();
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
        cmd.args(["bash", "-c", RECORDED_LAUNCH, "sh"])
            .arg(runtime.launch_identity_path(true))
            .arg(&runtime.program)
            .args(["--mode", "daemon", "--daemon-socket"])
            .arg(runtime.socket());
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-c", RECORDED_LAUNCH, "sh"])
            .arg(runtime.launch_identity_path(true))
            .arg(&runtime.program)
            .args(["--mode", "daemon", "--daemon-socket"])
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
                index + 1..index + 1,
                [
                    "bash".into(),
                    "-c".into(),
                    RECORDED_LAUNCH.into(),
                    "sh".into(),
                    runtime
                        .launch_identity_path(false)
                        .to_string_lossy()
                        .into_owned(),
                ],
            );
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
        Some(_) => tokio::process::Command::new(&argv[0]),
        None => {
            let mut command = tokio::process::Command::new("bash");
            command
                .args(["-c", RECORDED_LAUNCH, "sh"])
                .arg(runtime.launch_identity_path(false));
            command
        }
    };
    frontend
        .args(if runtime.container.is_none() {
            &argv[..]
        } else {
            &argv[1..]
        })
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
    #[serial_test::serial]
    fn guard_drop_does_not_wait_for_concurrent_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let prime = PrimeRuntime::new("/bin/false".into(), root.path().into(), None);
        let observation = prime.observation.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _lock = observation.lock().unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).is_ok()
        });
        ready_rx.recv().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let responsive = runtime.block_on(async {
            drop(PrimeCleanupGuard(Some(prime), None));
            release_tx.send(()).is_ok()
        });
        assert!(
            owner.join().unwrap() && responsive,
            "Drop blocked the async executor behind cleanup"
        );
        drop(runtime);
    }

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
        let mut runtime = PrimeRuntime::new("/bin/false".into(), root.path().into(), None);
        runtime.directory = root.path().into();
        let payload = format!(
            "bash -c {}",
            crate::session::environment::shell_escape_script_word(
                "trap '' TERM; printf 'ready\n'; while IFS= read -r line; do :; done"
            )
        );
        let spawn = |role: Option<bool>| {
            let command = match role {
                Some(daemon) => runtime.recorded_launch_command(&payload, daemon),
                None => payload.clone(),
            };
            let mut child = Waiter(
                Command::new("bash")
                    .args(["-c", &command])
                    .env("TMPDIR", root.path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut ready = String::new();
            std::io::BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            child
        };
        let mut frontend = spawn(Some(false));
        let mut daemon = spawn(Some(true));
        let mut foreign = spawn(None);
        let fence = |identity: &crate::process::PrimeProcessIdentity| {
            let mut command = Command::new("bash");
            command
                .args(["-c", CONTAINER_PROCESS_FENCE, "sh"])
                .arg(identity.pid.to_string())
                .arg(&identity.start_id);
            crate::session::capture::run_with_timeout_limit(
                command,
                Duration::from_secs(5),
                "launch fence regression",
                1024,
            )
            .unwrap();
        };
        let identity = runtime.launch_identity(false).unwrap().unwrap();
        let mut reused = identity.clone();
        reused.start_id = "proc:0".into();
        fence(&reused);
        assert!(frontend.0.try_wait().unwrap().is_none());
        fence(&identity);
        assert!(!frontend.0.wait().unwrap().success());
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(foreign.0.try_wait().unwrap().is_none());
        fence(&runtime.launch_identity(true).unwrap().unwrap());
        assert!(!daemon.0.wait().unwrap().success());
        assert!(foreign.0.try_wait().unwrap().is_none());
    }
}
