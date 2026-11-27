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

const PRIME_GUEST_CONTROL: &str = include_str!("../../assets/prime-control.py");

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
    #[serde(default)]
    pub(crate) proof_path: Option<PathBuf>,
    #[serde(skip)]
    observation: Arc<Mutex<PrimeRuntimeState>>,
}

#[derive(Serialize, Deserialize)]
struct PrimeCleanupProof {
    route: String,
    processes: Vec<crate::process::PrimeProcessIdentity>,
    settled: bool,
    #[serde(default)]
    epoch: Option<String>,
    #[serde(default)]
    launch_epoch: Option<String>,
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
            && self.proof_path == other.proof_path
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
    pub fn new(
        program: PathBuf,
        agent_dir: PathBuf,
        container: Option<PrimeContainer>,
    ) -> Result<Self> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("aoe-prime-{}", &id[..12]);
        let workers = crate::process::worker_registry::workers_dir()?.canonicalize()?;
        Ok(Self {
            directory: PathBuf::from("/tmp").join(&name),
            program,
            agent_dir,
            container,
            proof_path: Some(workers.join(format!(".{name}.prime-proof"))),
            observation: Arc::default(),
        })
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
        let _lock = self.lock_proof()?;
        let epoch = self.process_epoch()?;
        let proof = PrimeCleanupProof {
            route: self.proof_route()?,
            processes: Vec::new(),
            settled: false,
            epoch: Some(epoch.clone()),
            launch_epoch: Some(epoch),
        };
        if self.container.is_some() {
            return self.prepare_guest(&proof);
        }
        use std::os::unix::fs::{symlink, DirBuilderExt};
        let cleanup = self.directory.join("cleanup");
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700).create(&self.directory)?;
        let result = (|| {
            builder.create(&cleanup)?;
            symlink(
                self.agent_dir.join("daemon-workers"),
                cleanup.join("daemon-workers"),
            )?;
            self.write_proof(&proof)
        })();
        if let Err(error) = result {
            return match std::fs::remove_dir_all(&self.directory) {
                Ok(()) => Err(error),
                Err(rollback) => {
                    Err(error.context(format!("Prime setup rollback failed: {rollback}")))
                }
            };
        }
        Ok(())
    }

    fn guest_control(&self, action: &str) -> Result<Command> {
        let container = self
            .container
            .as_ref()
            .context("Prime guest route missing")?;
        let mut command = container.transport.command(&[]);
        command.args([
            "exec",
            "-i",
            &container.id,
            "python3",
            "-c",
            PRIME_GUEST_CONTROL,
            action,
        ]);
        Ok(command)
    }

    fn prepare_guest(&self, proof: &PrimeCleanupProof) -> Result<()> {
        use std::io::{BufRead as _, Read as _, Write as _};
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut command = self.guest_control("prepare")?;
        command
            .arg(&self.directory)
            .arg(self.agent_dir.join("daemon-workers"))
            .arg(&token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("Prime guest setup spawn failed")?;
        let stdout = child
            .stdout
            .take()
            .context("Prime guest setup output missing")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout.take(1024));
            for _ in 0..2 {
                let mut line = String::new();
                let result = reader.read_line(&mut line).map(|_| line);
                if tx.send(result).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        let result = (|| {
            let line = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
            anyhow::ensure!(line == "ready\n", "Prime guest setup readiness unproven");
            ready = true;
            self.write_proof(proof)?;
            let mut stdin = child
                .stdin
                .take()
                .context("Prime guest commit input missing")?;
            writeln!(stdin, "{token}")?;
            drop(stdin);
            let line = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
            anyhow::ensure!(line == "committed\n", "Prime guest setup commit unproven");
            loop {
                if let Some(status) = child.try_wait()? {
                    anyhow::ensure!(status.success(), "Prime guest setup failed");
                    return Ok(());
                }
                anyhow::ensure!(Instant::now() < deadline, "Prime guest setup timed out");
                std::thread::sleep(Duration::from_millis(25));
            }
        })();
        if let Err(error) = result {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
            if ready {
                let mut rollback = self.guest_control("rollback")?;
                rollback.arg(&self.directory).arg(&token);
                if let Err(rollback) = crate::session::capture::run_with_timeout_limit(
                    rollback,
                    Duration::from_secs(5),
                    "Prime guest setup rollback",
                    1024,
                ) {
                    return Err(error.context(format!("Prime setup rollback unproven: {rollback}")));
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn observe(&self, fresh: bool) -> Option<Arc<PrimeObservation>> {
        self.validate().ok()?;
        let mut cache = self.observation.try_lock().ok()?;
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

    fn control_path(&self) -> Result<&Path> {
        let path = self
            .proof_path
            .as_deref()
            .context("Prime control path is unattested")?;
        anyhow::ensure!(path.is_absolute(), "Prime control path must be absolute");
        let name = self
            .directory
            .file_name()
            .context("missing Prime runtime name")?;
        let actual = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix('.'))
            .and_then(|name| name.strip_suffix(".prime-proof"));
        anyhow::ensure!(
            actual.is_some_and(|actual| std::ffi::OsStr::new(actual) == name),
            "Prime control path does not match the runtime"
        );
        Ok(path)
    }

    fn lock_proof(&self) -> Result<std::fs::File> {
        use fs2::FileExt as _;
        let path = self.control_path()?.with_extension("prime-lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    pub(crate) fn proof_route(&self) -> Result<String> {
        #[derive(Serialize)]
        struct NativeRoute<'a> {
            directory: &'a Path,
            program: &'a Path,
            agent_dir: &'a Path,
            container: &'a Option<PrimeContainer>,
        }
        Ok(sha256_prefix(
            &serde_json::to_vec(&NativeRoute {
                directory: &self.directory,
                program: &self.program,
                agent_dir: &self.agent_dir,
                container: &self.container,
            })?,
            32,
        ))
    }

    fn read_proof(&self) -> Result<PrimeCleanupProof> {
        let path = self.control_path()?;
        let proof: PrimeCleanupProof = serde_json::from_slice(
            &std::fs::read(path).context("Prime cleanup ownership ledger unavailable")?,
        )?;
        anyhow::ensure!(
            proof.route == self.proof_route()?,
            "Prime cleanup ledger belongs to another execution route"
        );
        Ok(proof)
    }

    pub(crate) fn attest_control_path(&mut self, workers: &Path) -> Result<()> {
        self.validate()?;
        let name = self
            .directory
            .file_name()
            .context("missing Prime runtime name")?
            .to_string_lossy();
        self.proof_path = Some(workers.join(format!(".{name}.prime-proof")));
        let result = (|| {
            let _lock = self.lock_proof()?;
            self.read_proof().map(|_| ())
        })();
        if result.is_err() {
            self.proof_path = None;
        }
        result
    }

    fn write_proof(&self, proof: &PrimeCleanupProof) -> Result<()> {
        let path = self.control_path()?;
        crate::session::atomic_write(path, &serde_json::to_vec(proof)?)
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
            let command = if container.transport.kind
                == crate::session::ContainerRuntimeName::AppleContainer
            {
                let mut command = self.guest_control("descriptor-archive")?;
                command.arg(&directory);
                command
            } else {
                let mut command = container.transport.command(&[]);
                command
                    .arg("cp")
                    .arg(format!("{}:{}/.", container.id, directory.display()))
                    .arg("-");
                command
            };
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
rows=()
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
    printf -v row '%s\tproc:%s\t%s\t%s' "$pid" "${fields[19]}" "${fields[1]}" "$marked"
    rows+=("$row")
    seen+="$pid"$'\t'"proc:${fields[19]}"$'\n'
done
while IFS=$'\t' read -r pid start; do
    [[ -z $pid || ! -d /proc/$pid ]] && continue
    IFS= read -r stat < /proc/$pid/stat || { [[ ! -d /proc/$pid ]] && continue; exit 1; }
    read -r -a fields <<< "${stat##*) }"
    [[ ${fields[0]} == Z || "proc:${fields[19]}" != "$start" ]] && continue
    [[ $seen == *$'\n'"$pid"$'\t'"$start"$'\n'* ]] || exit 1
done <<< "$known"
for row in "${rows[@]}"; do
    IFS=$'\t' read -r pid start parent marked <<< "$row"
    if ! IFS= read -r stat < "/proc/$pid/stat"; then
        [[ ! -d /proc/$pid ]] && continue
        [[ $marked == 0 && $known != *$'\n'"$pid"$'\t'* ]] || exit 1
        continue
    fi
    read -r -a fields <<< "${stat##*) }"
    [[ ${fields[0]} != Z && "proc:${fields[19]}" == "$start" ]] || continue
    printf '%s\n' "$row"
done"#;
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
        if self.container.is_none() {
            return Ok(());
        }
        let Some(identity) = self.launch_identity(daemon)? else {
            return Ok(());
        };
        let mut command = self.guest_control("fence")?;
        command
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

    fn process_epoch(&self) -> Result<String> {
        if self.container.is_none() {
            return crate::process::boot_id().context("Prime process epoch unavailable");
        }
        let bytes = crate::session::capture::run_with_timeout_limit(
            self.guest_control("epoch")?,
            Duration::from_secs(2),
            "Prime guest process epoch",
            1024,
        )?;
        let value = String::from_utf8(bytes)?;
        anyhow::ensure!(!value.is_empty(), "Prime guest process epoch is empty");
        Ok(value)
    }

    fn fence_owned_processes(
        &self,
        identities: &[crate::process::PrimeProcessIdentity],
    ) -> Result<()> {
        if self.container.is_none() {
            return crate::process::fence_processes(identities, Duration::from_millis(500));
        }
        let mut command = self.guest_control("fence-many")?;
        for identity in identities {
            command
                .arg(identity.pid.to_string())
                .arg(&identity.start_id);
        }
        let output = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(5),
            "Prime owned auxiliary fence",
            1024,
        )?;
        anyhow::ensure!(output == b"settled", "Prime auxiliary termination unproven");
        Ok(())
    }

    /// Check the frozen route's termination receipt without signalling or recapturing processes.
    pub(crate) fn verify_stopped(&self) -> Result<()> {
        self.validate()?;
        let _lock = self.lock_proof()?;
        let proof = self.read_proof()?;
        anyhow::ensure!(
            proof.settled && proof.processes.is_empty() && proof.epoch.is_some(),
            "Prime native termination remains unproven"
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
        let _lock = self.lock_proof()?;
        let mut proof = self.read_proof()?;
        if let Some(container) = &self.container {
            if !container.transport.container_exists(&container.id)? {
                proof.processes.clear();
                proof.settled = true;
                self.write_proof(&proof)?;
                state.observation = None;
                state.cleaned = true;
                return Ok(());
            }
        }
        let exists = self.directory_exists(proof.settled)?;
        let epoch = self.process_epoch()?;
        if proof
            .epoch
            .as_ref()
            .is_some_and(|previous| previous != &epoch)
        {
            proof.processes = self.process_snapshot(&[])?;
            proof.epoch = Some(epoch.clone());
            proof.settled = false;
            self.write_proof(&proof)?;
        }
        let launch_attested = proof.launch_epoch.as_ref() == Some(&epoch);
        if !proof.settled {
            if proof.epoch.is_none() {
                let fresh = self.process_snapshot(&[])?;
                let observed = self.process_snapshot(&proof.processes)?;
                anyhow::ensure!(observed.iter().all(|identity| fresh.contains(identity)), "legacy Prime ledger cannot attest live process incarnations; retained ownership evidence");
                proof.processes = fresh;
            }
            if launch_attested || proof.launch_epoch.is_none() {
                let mut sources = self.worker_identities()?;
                if exists {
                    sources.extend(self.launch_identity(true)?);
                    sources.extend(self.launch_identity(false)?);
                }
                if !launch_attested {
                    let fresh = self.process_snapshot(&[])?;
                    let observed = self.process_snapshot(&sources)?;
                    anyhow::ensure!(observed.iter().all(|identity| fresh.contains(identity)), "legacy Prime launch evidence cannot attest live process incarnations; retained ownership evidence");
                    sources = fresh;
                }
                proof.processes.extend(sources);
            }
            proof.epoch = Some(epoch);
            proof.processes = self.process_snapshot(&proof.processes)?;
            self.write_proof(&proof)?;
            if !exists {
                anyhow::ensure!(proof.processes.is_empty(), "Prime runtime directory is missing but owned processes remain; retained ownership ledger");
                proof.settled = true;
                self.write_proof(&proof)?;
            }
        }
        if !proof.settled {
            if launch_attested {
                self.fence_container_processes(false)?;
            }
            if launch_attested {
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
            }
            state.observation = None;
            if launch_attested {
                self.stop_owned_daemon()?;
            }
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
                self.fence_owned_processes(&proof.processes)?;
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
    #[arg(long)]
    pub rewind_container_env: bool,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub async fn run(args: PrimeRunnerArgs) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let runtime: PrimeRuntime = serde_json::from_str(&args.runtime)?;
    runtime.prepare()?;
    let mut guard = PrimeCleanupGuard(Some(runtime.clone()), None);
    let mut owned_daemon = None;
    let result: Result<()> = async {
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
        let daemon = owned_daemon.insert(daemon.spawn().context("starting owned Prime daemon")?);
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
        if args.rewind_container_env {
            // Darwin /dev/fd opens share the daemon client's consumed offset.
            let offset = unsafe {
                libc::lseek(
                    i32::from(crate::session::environment::CONTAINER_EXEC_ENV_FD),
                    0,
                    libc::SEEK_SET,
                )
            };
            if offset == -1 {
                return Err(std::io::Error::last_os_error())
                    .context("rewinding protected Prime container environment");
            }
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
        if let Some(status) = status {
            anyhow::ensure!(status.success(), "Prime frontend exited with {status}");
        }
        Ok(())
    }
    .await;
    let owned = runtime.clone();
    let cleanup = match tokio::task::spawn_blocking(move || owned.cleanup()).await {
        Ok(result) => result,
        Err(error) => Err(error.into()),
    };
    if cleanup.is_ok() {
        guard.0.take();
    }
    if let Some(mut daemon) = owned_daemon {
        daemon.start_kill().ok();
        daemon.wait().await.ok();
    }
    match (result, cleanup) {
        (Err(primary), Err(cleanup)) => {
            Err(primary.context(format!("Prime cleanup also failed: {cleanup:#}")))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn removed_container_settles_only_after_a_definitive_frozen_transport_probe() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let executable = root.path().join("runtime");
        std::fs::write(&executable, r#"#!/bin/sh
case "$1" in
  container|logs)
    case "$CASE" in
      missing) echo 'Error: container not found: pinned; No such container: pinned' >&2; exit 1 ;;
      daemon) echo 'Cannot connect to the Docker daemon; unable to connect to Podman socket; failed to connect to container daemon; container not found; No such container: pinned' >&2; exit 1 ;;
      permission) echo 'permission denied while trying to connect to the docker daemon socket; permission denied accessing container socket; container not found; No such container: pinned' >&2; exit 1 ;;
      ambiguous) echo 'container with ID pinned failed to execute' >&2; exit 1 ;;
      present) exit 0 ;;
    esac ;;
esac
echo 'guest execution refused' >&2
exit 42
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        for kind in [
            crate::session::ContainerRuntimeName::Docker,
            crate::session::ContainerRuntimeName::Podman,
            crate::session::ContainerRuntimeName::AppleContainer,
        ] {
            for case in [
                "missing",
                "daemon",
                "permission",
                "ambiguous",
                "present",
                "not-installed",
            ] {
                let runtime = PrimeRuntime::new(
                    "/bin/false".into(),
                    root.path().join("agent"),
                    Some(PrimeContainer {
                        transport: crate::containers::RuntimeExecutionSnapshot {
                            kind,
                            program: if case == "not-installed" {
                                root.path().join("absent-binary")
                            } else {
                                executable.clone()
                            },
                            cwd: root.path().into(),
                            endpoint: "frozen".into(),
                            local_mounts: false,
                            routing: vec![("CASE".into(), Some(case.into()))],
                            global_arguments: Vec::new(),
                        },
                        id: "pinned".into(),
                        cwd: root.path().into(),
                    }),
                )
                .unwrap();
                runtime
                    .write_proof(&PrimeCleanupProof {
                        route: runtime.proof_route().unwrap(),
                        processes: vec![crate::process::PrimeProcessIdentity {
                            pid: std::process::id(),
                            start_id: "guest:obsolete".into(),
                        }],
                        settled: false,
                        epoch: Some("original-guest".into()),
                        launch_epoch: Some("original-guest".into()),
                    })
                    .unwrap();
                let original = std::fs::read(runtime.control_path().unwrap()).unwrap();
                let restored: PrimeRuntime =
                    serde_json::from_slice(&serde_json::to_vec(&runtime).unwrap()).unwrap();
                let result = restored.cleanup();
                if case == "missing" {
                    result.unwrap();
                    let proof = restored.read_proof().unwrap();
                    assert!(proof.settled);
                    assert!(proof.processes.is_empty());
                    assert_eq!(proof.epoch.as_deref(), Some("original-guest"));
                    let fresh: PrimeRuntime =
                        serde_json::from_slice(&serde_json::to_vec(&restored).unwrap()).unwrap();
                    fresh.cleanup().unwrap();
                } else {
                    assert!(result.is_err(), "{kind:?}/{case}");
                    assert_eq!(
                        std::fs::read(restored.control_path().unwrap()).unwrap(),
                        original
                    );
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    struct CleanupProbe(std::process::Child);

    #[cfg(target_os = "linux")]
    impl Drop for CleanupProbe {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "linux")]
    impl CleanupProbe {
        fn spawn(directory: &Path, ignore_term: bool) -> Self {
            use std::io::{BufRead as _, Write as _};
            use std::os::unix::process::CommandExt as _;
            let mut command = Command::new("cat");
            command
                .env("TMPDIR", directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if ignore_term && libc::signal(libc::SIGTERM, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = Self(command.spawn().unwrap());
            child
                .0
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"probe-ready\n")
                .unwrap();
            let stdout = child.0.stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut ready = String::new();
                let result = std::io::BufReader::new(stdout)
                    .read_line(&mut ready)
                    .map(|_| ready);
                let _ = tx.send(result);
            });
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
                "probe-ready\n"
            );
            reader.join().unwrap();
            child
        }

        fn identity(&self) -> crate::process::PrimeProcessIdentity {
            crate::process::PrimeProcessIdentity {
                pid: self.0.id(),
                start_id: crate::process::process_birth_identity(self.0.id())
                    .unwrap()
                    .unwrap()
                    .start_id,
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial]
    fn missing_runtime_requires_external_descriptor_settlement_before_retirement() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let runtime =
            PrimeRuntime::new("/bin/false".into(), root.path().join("agent"), None).unwrap();
        runtime.prepare().unwrap();
        let mut worker = CleanupProbe::spawn(root.path(), false);
        let identity = worker.identity();
        let descriptors = runtime.worker_descriptor_dir();
        std::fs::create_dir_all(&descriptors).unwrap();
        std::fs::write(descriptors.join("worker.json"), serde_json::to_vec(&serde_json::json!({"pid":identity.pid, "processStartId":identity.start_id, "supervisorSocketPath":runtime.socket()})).unwrap()).unwrap();
        let route = serde_json::to_vec(&runtime).unwrap();
        std::fs::remove_dir_all(&runtime.directory).unwrap();
        assert!(runtime.cleanup().is_err());
        assert!(worker.0.try_wait().unwrap().is_none());
        let proof = runtime.read_proof().unwrap();
        assert!(!proof.settled);
        assert!(proof.processes.contains(&identity));
        drop(worker.0.stdin.take());
        assert!(worker.0.wait().unwrap().success());
        let restored: PrimeRuntime = serde_json::from_slice(&route).unwrap();
        restored.cleanup().unwrap();
        assert!(restored.read_proof().unwrap().settled);
        assert!(!restored.directory.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial]
    fn old_epoch_never_reimports_stale_launch_or_descriptor_pids_on_retry() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let runtime =
            PrimeRuntime::new("/bin/false".into(), root.path().join("agent"), None).unwrap();
        runtime.prepare().unwrap();
        let mut foreign = CleanupProbe::spawn(root.path(), false);
        let identity = foreign.identity();
        let mut fresh = CleanupProbe::spawn(&runtime.directory, false);
        let mut proof = runtime.read_proof().unwrap();
        proof.epoch = Some("previous-boot".into());
        proof.launch_epoch = proof.epoch.clone();
        proof.processes = vec![identity.clone()];
        runtime.write_proof(&proof).unwrap();
        std::fs::write(
            runtime.launch_identity_path(true),
            format!("{}\t{}\n", identity.pid, identity.start_id),
        )
        .unwrap();
        let descriptors = runtime.worker_descriptor_dir();
        std::fs::create_dir_all(&descriptors).unwrap();
        std::fs::write(descriptors.join("worker.json"), serde_json::to_vec(&serde_json::json!({"pid":identity.pid, "processStartId":identity.start_id, "supervisorSocketPath":runtime.socket()})).unwrap()).unwrap();
        let route = serde_json::to_vec(&runtime).unwrap();
        runtime.cleanup().unwrap();
        assert!(!fresh.0.wait().unwrap().success());
        assert!(foreign.0.try_wait().unwrap().is_none());
        let restored: PrimeRuntime = serde_json::from_slice(&route).unwrap();
        restored.cleanup().unwrap();
        assert!(restored.read_proof().unwrap().settled);
        assert_eq!(
            restored.read_proof().unwrap().launch_epoch.as_deref(),
            Some("previous-boot")
        );
        assert!(foreign.0.try_wait().unwrap().is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial]
    fn native_shutdown_fences_scoped_setsid_auxiliaries_and_preserves_foreign_processes() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let program = root.path().join("native-shutdown");
        std::fs::write(
            &program,
            r#"#!/bin/sh
printf '{"failed":[]}\n'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        for ignore_term in [false, true] {
            let runtime =
                PrimeRuntime::new(program.clone(), root.path().join("agent"), None).unwrap();
            runtime.prepare().unwrap();
            let mut auxiliary = CleanupProbe::spawn(&runtime.directory, ignore_term);
            let mut foreign = CleanupProbe::spawn(root.path(), false);
            let mut wrong = auxiliary.identity();
            wrong.start_id = "proc:0".into();
            crate::process::fence_processes(&[wrong], Duration::ZERO).unwrap();
            assert!(auxiliary.0.try_wait().unwrap().is_none());
            let route = serde_json::to_vec(&runtime).unwrap();
            runtime.cleanup().unwrap();
            assert!(!auxiliary.0.wait().unwrap().success());
            assert!(foreign.0.try_wait().unwrap().is_none());
            assert!(!runtime.directory.exists());
            let restored: PrimeRuntime = serde_json::from_slice(&route).unwrap();
            restored.cleanup().unwrap();
            assert!(restored.read_proof().unwrap().settled);
            assert!(foreign.0.try_wait().unwrap().is_none());
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    #[serial_test::serial]
    fn guest_auxiliary_batch_settles_term_resistant_processes_within_the_caller_deadline() {
        let root = tempfile::tempdir().unwrap();
        let mut processes: Vec<_> = (0..12)
            .map(|_| CleanupProbe::spawn(root.path(), true))
            .collect();
        let mut foreign = CleanupProbe::spawn(root.path(), false);
        let mut command = Command::new("python3");
        command.args(["-c", PRIME_GUEST_CONTROL, "fence-many"]);
        for process in &processes {
            let identity = process.identity();
            command.arg(identity.pid.to_string()).arg(identity.start_id);
        }
        let output = crate::session::capture::run_with_timeout_limit(
            command,
            Duration::from_secs(5),
            "owned guest auxiliary batch",
            1024,
        )
        .unwrap();
        assert_eq!(output, b"settled");
        for process in &mut processes {
            assert!(!process.0.wait().unwrap().success());
        }
        assert!(foreign.0.try_wait().unwrap().is_none());
    }
    #[test]
    #[serial_test::serial]
    fn apple_descriptor_snapshot_keeps_namespace_and_file_type_guards() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let transport = root.path().join("apple-exec");
        std::fs::write(&transport, "#!/bin/sh\n[ \"$1\" = exec ] || exit 84\nshift\n[ \"$1\" != -i ] || shift\n[ \"$1\" = pinned ] || exit 85\nshift\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&transport, std::fs::Permissions::from_mode(0o700)).unwrap();
        let prime = PrimeRuntime::new(
            "/bin/false".into(),
            root.path().join("agent"),
            Some(PrimeContainer {
                transport: crate::containers::RuntimeExecutionSnapshot {
                    kind: crate::session::ContainerRuntimeName::AppleContainer,
                    program: transport,
                    cwd: root.path().into(),
                    endpoint: "apple-contract".into(),
                    local_mounts: false,
                    routing: Vec::new(),
                    global_arguments: Vec::new(),
                },
                id: "pinned".into(),
                cwd: root.path().into(),
            }),
        )
        .unwrap();
        let directory = prime.worker_descriptor_dir();
        std::fs::create_dir_all(&directory).unwrap();
        let descriptor = directory.join("worker.json");
        let own = serde_json::json!({"pid":42,"processStartId":"proc:100", "supervisorSocketPath":prime.socket()});
        std::fs::write(&descriptor, serde_json::to_vec(&own).unwrap()).unwrap();
        let identities = prime.worker_identities().unwrap();
        assert_eq!(
            identities,
            vec![crate::process::PrimeProcessIdentity {
                pid: 42,
                start_id: "proc:100".into()
            }]
        );
        for invalid in [
            serde_json::json!({"pid":42,"processStartId":"proc:100", "supervisorSocketPath":"/tmp/foreign.sock"}),
            serde_json::json!({"pid":u32::MAX,"processStartId":"proc:100", "supervisorSocketPath":prime.socket()}),
            serde_json::json!({"pid":42,"supervisorSocketPath":prime.socket()}),
        ] {
            std::fs::write(&descriptor, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(prime.worker_identities().is_err(), "{invalid}");
        }
        std::fs::remove_file(&descriptor).unwrap();
        let foreign = root.path().join("foreign.json");
        std::fs::write(&foreign, serde_json::to_vec(&own).unwrap()).unwrap();
        std::os::unix::fs::symlink(&foreign, &descriptor).unwrap();
        assert!(prime.worker_identities().is_err());
        std::fs::remove_file(&descriptor).unwrap();
        std::fs::create_dir(&descriptor).unwrap();
        std::fs::write(
            descriptor.join("nested.json"),
            serde_json::to_vec(&own).unwrap(),
        )
        .unwrap();
        assert!(prime.worker_identities().is_err());
        std::fs::remove_dir_all(&descriptor).unwrap();
        assert_eq!(prime.worker_identities().unwrap(), Vec::new());
        assert!(!root.path().join("-").exists());
    }

    #[test]
    #[serial_test::serial]
    fn restored_control_path_survives_agent_environment_and_failed_setup_rolls_back_only_its_root()
    {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let runtime = PrimeRuntime::new("/bin/false".into(), root.path().into(), None).unwrap();
        let saved = serde_json::to_vec(&runtime).unwrap();
        let invalid_config = root.path().join("not-a-directory");
        std::fs::write(&invalid_config, b"occupied").unwrap();
        let _environment =
            crate::session::test_support::EnvGuard::set(&[("XDG_CONFIG_HOME", &invalid_config)]);
        let restored: PrimeRuntime = serde_json::from_slice(&saved).unwrap();
        restored.prepare().unwrap();
        assert_eq!(
            restored.read_proof().unwrap().route,
            restored.proof_route().unwrap()
        );
        std::fs::remove_dir_all(&restored.directory).unwrap();
        let proof = restored.proof_path.as_ref().unwrap();
        std::fs::remove_file(proof).unwrap();
        std::fs::create_dir(proof).unwrap();
        assert!(restored.prepare().is_err());
        assert!(!restored.directory.exists());
        std::fs::create_dir(&restored.directory).unwrap();
        let marker = restored.directory.join("existing-owner");
        std::fs::write(&marker, b"retained").unwrap();
        assert!(restored.prepare().is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), b"retained");
        std::fs::remove_dir_all(&restored.directory).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn guard_drop_does_not_wait_for_concurrent_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let prime = PrimeRuntime::new("/bin/false".into(), root.path().into(), None).unwrap();
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
    #[serial_test::serial]
    fn observation_does_not_wait_for_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let prime = PrimeRuntime::new("/bin/false".into(), root.path().into(), None).unwrap();
        for fresh in [false, true] {
            let observation = prime.observation.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let owner = std::thread::spawn(move || {
                let _state = observation.lock().unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).is_ok()
            });
            ready_rx.recv().unwrap();
            assert!(prime.observe(fresh).is_none());
            let responsive = release_tx.send(()).is_ok();
            assert!(
                owner.join().unwrap() && responsive,
                "observation waited for cleanup"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn startup_error_waits_for_cleanup_and_retains_unsettled_ownership() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        for cleanup_fails in [false, true] {
            let program = root.path().join(format!("native-{cleanup_fails}"));
            std::fs::write(&program, format!(
                "#!/bin/sh\ncase $1 in\nlist) exit 1;;\nshutdown) : > \"$0.started\"; while [ ! -f \"$0.release\" ]; do sleep 0.01; done; {};;\nesac\n",
                if cleanup_fails { "exit 42" } else { "printf \"%s\" '{\"failed\":[]}'" }
            )).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let started = program.with_file_name(format!("native-{cleanup_fails}.started"));
            let release = program.with_file_name(format!("native-{cleanup_fails}.release"));
            struct Release(std::path::PathBuf);
            impl Drop for Release {
                fn drop(&mut self) {
                    let _ = std::fs::write(&self.0, b"");
                }
            }
            let prime = PrimeRuntime::new(program, root.path().into(), None).unwrap();
            struct PrivateRoot(std::path::PathBuf);
            impl Drop for PrivateRoot {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let _private_root = PrivateRoot(prime.directory.clone());
            let args = PrimeRunnerArgs {
                runtime: serde_json::to_string(&prime).unwrap(),
                daemon_command: Some("exit 1".into()),
                rewind_container_env: false,
                command: vec!["/bin/false".into()],
            };
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            executor.block_on(async {
                let _release_on_drop = Release(release.clone());
                let running = tokio::spawn(run(args));
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while !started.exists() {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "cleanup did not start"
                    );
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let returned_before_cleanup = running.is_finished();
                std::fs::write(&release, b"").unwrap();
                assert!(running.await.unwrap().is_err());
                assert!(
                    !returned_before_cleanup,
                    "startup error returned before native cleanup finished"
                );
                assert_eq!(prime.directory.exists(), cleanup_fails);
                assert_eq!(prime.read_proof().unwrap().settled, !cleanup_fails);
            });
            drop(executor);
        }
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
    #[serial_test::serial]
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
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let mut runtime = PrimeRuntime::new("/bin/false".into(), root.path().into(), None).unwrap();
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
            let mut command = Command::new("python3");
            command
                .args(["-c", PRIME_GUEST_CONTROL, "fence"])
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
