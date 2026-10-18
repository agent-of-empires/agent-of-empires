//! Linux-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = false;
use anyhow::Context as _;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

fn parse_kernel_process_stat(stat: &[u8]) -> anyhow::Result<super::KernelProcessState> {
    use anyhow::Context as _;

    let close_paren = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .context("malformed kernel process stat")?;
    let mut fields = stat[close_paren + 1..].split_ascii_whitespace();
    let state = fields.next().context("kernel process state absent")?;
    anyhow::ensure!(
        state.len() == 1 && state[0].is_ascii_alphabetic(),
        "invalid kernel process state"
    );
    let group: u32 =
        std::str::from_utf8(fields.nth(1).context("kernel process group absent")?)?.parse()?;
    let threads: u32 = std::str::from_utf8(
        fields
            .nth(14)
            .context("kernel process thread count absent")?,
    )?
    .parse()?;
    anyhow::ensure!(threads > 0, "invalid kernel process thread count");
    let start_id: u64 =
        std::str::from_utf8(fields.nth(1).context("kernel process birth absent")?)?.parse()?;
    Ok(super::KernelProcessState {
        start_id,
        group,
        writing: state[0] != b'Z' || threads > 1,
    })
}

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

pub(crate) fn profile_directory_identity(path: &Path) -> anyhow::Result<Option<DirectoryIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(metadata.is_dir(), "profile is not a directory");
    Ok(Some(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

pub(super) use super::unix::{
    close_frozen_payload, configure_process_group, exec_command, kill_process_group,
    restore_environment_value, terminate_process_group,
};

pub(super) fn launch_executable(
    executable: std::path::PathBuf,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    if executable.try_exists()? {
        return Ok(executable);
    }
    let replacement = executable
        .as_os_str()
        .as_bytes()
        .strip_suffix(b" (deleted)")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "AoE executable is unavailable",
            )
        })?;
    which::which(Path::new(std::ffi::OsStr::from_bytes(replacement)))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::NotFound, error))
}
pub(super) fn rename_exclusive(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use nix::NixPath;
    use std::os::fd::AsRawFd;

    let result = source.with_nix_path(|source| {
        destination.with_nix_path(|destination| {
            // SAFETY: both descriptors remain owned for the call and NixPath
            // supplies live NUL-terminated names. The syscall also supports musl.
            nix::errno::Errno::result(unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    destination_dir.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            })
            .map(|_| ())
        })
    })??;
    result.map_err(std::io::Error::from)
}

pub(super) fn connect_systemd_socket(
    socket: &std::os::unix::net::UnixDatagram,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    if let Some(name) = destination.as_bytes().strip_prefix(b"@") {
        socket.connect_addr(&std::os::unix::net::SocketAddr::from_abstract_name(name)?)
    } else {
        socket.connect(destination)
    }
}

pub(super) fn collect_pid_tree(pid: u32) -> Vec<u32> {
    let children_map = build_children_map();
    let mut pids = vec![pid];
    super::collect_descendants_from_map(pid, &children_map, &mut pids);
    pids
}

pub(super) fn build_children_map() -> HashMap<u32, Vec<u32>> {
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    let proc_dir = Path::new("/proc");
    let Ok(entries) = fs::read_dir(proc_dir) else {
        return children_map;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let Ok(child_pid) = name_str.parse::<u32>() else {
            continue;
        };

        let stat_path = entry.path().join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            continue;
        };

        if let Some(ppid) = parse_stat_field(&content, 3) {
            children_map.entry(ppid as u32).or_default().push(child_pid);
        }
    }

    children_map
}

/// Environment entries are compared NUL-delimited, so there is no prefix collision.
/// `environ` is owner-only, so only same-uid processes can match on it.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
) -> Vec<bool> {
    let n = env_needles.len();
    let mut found = vec![false; n];
    let mut remaining = n;
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        if remaining == 0 {
            break;
        }
        let name = entry.file_name();
        if name.to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let dir = entry.path();

        let environ_raw = fs::read(dir.join("environ")).unwrap_or_default();
        let environ = String::from_utf8_lossy(&environ_raw);
        let env_entries: std::collections::HashSet<&str> =
            environ.split('\0').filter(|s| !s.is_empty()).collect();

        let cmd_raw = fs::read(dir.join("cmdline")).unwrap_or_default();
        let cmdline_raw = String::from_utf8_lossy(&cmd_raw);
        let cmd_tokens: Vec<&str> = cmdline_raw
            .split('\0')
            .filter(|value| !value.is_empty())
            .collect();
        let cmdline = cmdline_raw.replace('\0', " ");

        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_hit =
                !env_needles[i].is_empty() && env_entries.contains(env_needles[i].as_str());
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && cmdline.contains(s));
            let executable_hit = executable_needles[i].as_deref().is_some_and(|needle| {
                !needle.is_empty()
                    && cmd_tokens.iter().any(|token| {
                        std::path::Path::new(token)
                            .file_name()
                            .and_then(|value| value.to_str())
                            == Some(needle)
                    })
            });
            let has_env = !env_needles[i].is_empty();
            let has_cmd = cmdline_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let has_executable = executable_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let matched = (has_env || has_cmd || has_executable)
                && (!has_env || env_hit)
                && (!has_cmd || cmd_hit)
                && (!has_executable || executable_hit);
            if matched {
                found[i] = true;
                remaining -= 1;
            }
        }
    }
    found
}

fn process_disappeared(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn foreign_process_permission_error(error: &std::io::Error, owner: Option<u32>, euid: u32) -> bool {
    matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
        && owner.is_some_and(|uid| uid != euid)
}

fn runner_group_member_identity(
    pid: u32,
    stat: &[u8],
    group: u32,
) -> anyhow::Result<Option<super::PrimeProcessIdentity>> {
    let process = parse_kernel_process_stat(stat)?;
    if process.group != group || !process.writing {
        return Ok(None);
    }
    Ok(Some(super::PrimeProcessIdentity {
        pid,
        start_id: format!("proc:{}", process.start_id),
    }))
}

pub(super) fn runner_group_executing(group: u32) -> anyhow::Result<bool> {
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat = match fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) => {
                use std::os::unix::fs::MetadataExt as _;
                let owner = entry.metadata().ok().map(|metadata| metadata.uid());
                if foreign_process_permission_error(&error, owner, unsafe { libc::geteuid() }) {
                    continue;
                }
                return Err(error).context("runner group census unavailable");
            }
        };
        if runner_group_member_identity(pid, &stat, group)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn process_start_id(pid: u32) -> anyhow::Result<Option<String>> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error).context("reading process birth identity"),
    };
    let offset = stat
        .windows(2)
        .rposition(|pair| pair == b") ")
        .context("invalid process stat")?;
    let tail = std::str::from_utf8(&stat[offset + 2..])?;
    let mut fields = tail.split_ascii_whitespace();
    fields.next().context("missing process state")?;
    let start = fields.nth(18).context("missing process birth identity")?;
    Ok(Some(format!("proc:{start}")))
}

pub(super) struct ProcessHandle(std::os::fd::OwnedFd);
pub(super) fn process_group_id(pid: u32) -> anyhow::Result<Option<u32>> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let offset = stat
        .windows(2)
        .rposition(|pair| pair == b") ")
        .context("invalid process stat")?;
    let tail = std::str::from_utf8(&stat[offset + 2..])?;
    Ok(Some(
        tail.split_ascii_whitespace()
            .nth(2)
            .context("missing process group")?
            .parse()?,
    ))
}
impl ProcessHandle {
    pub(super) fn capture(identity: &super::PrimeProcessIdentity) -> anyhow::Result<Option<Self>> {
        Self::capture_identity(identity, false)
    }

    pub(super) fn capture_group(
        identity: &super::PrimeProcessIdentity,
    ) -> anyhow::Result<Option<Self>> {
        Self::capture_identity(identity, true)
    }

    fn capture_identity(
        identity: &super::PrimeProcessIdentity,
        allow_exited: bool,
    ) -> anyhow::Result<Option<Self>> {
        use std::os::fd::FromRawFd as _;
        let pid = i32::try_from(identity.pid)?;
        anyhow::ensure!(pid > 0, "invalid owned process PID");
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            let error = std::io::Error::last_os_error();
            if process_disappeared(&error) {
                return Ok(None);
            }
            return Err(error).context("opening owned process handle");
        }
        let handle = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) });
        if process_start_id(identity.pid)?.as_deref() != Some(&identity.start_id)
            || (!allow_exited && handle.exited()?)
        {
            return Ok(None);
        }
        Ok(Some(handle))
    }
    pub(super) fn group_executing(&self) -> anyhow::Result<Option<bool>> {
        match self.send_group(0) {
            Ok(()) => Ok(Some(true)),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(Some(false)),
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn signal_group(&self, signal: i32) -> anyhow::Result<()> {
        match self.send_group(signal) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn send_group(&self, signal: i32) -> std::io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // PIDFD_SIGNAL_PROCESS_GROUP binds the original group, including after leader reap.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                4,
            )
        };
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub(super) fn exited(&self) -> anyhow::Result<bool> {
        use std::os::fd::AsRawFd as _;
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("checking owned process exit");
        }
        Ok(result > 0)
    }

    pub(super) fn signal(&self, signal: i32) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd as _;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if !process_disappeared(&error) {
                return Err(error).context("signalling owned process handle");
            }
        }
        Ok(())
    }
}

pub(super) fn runner_group_identities(
    group: u32,
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut result = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let stat = match fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) => {
                let owner = entry.metadata().ok().map(|metadata| metadata.uid());
                if foreign_process_permission_error(&error, owner, unsafe { libc::geteuid() }) {
                    continue;
                }
                return Err(error).context("inspecting owned runner group");
            }
        };
        if let Some(identity) = runner_group_member_identity(pid, &stat, group)? {
            result.push(identity);
        }
    }
    Ok(result)
}

pub(super) fn prime_processes(
    marker: &str,
    socket: &Path,
    known: &[super::PrimeProcessIdentity],
) -> anyhow::Result<Vec<super::PrimeProcessIdentity>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let mut samples = Vec::new();
    let socket = socket.as_os_str().as_encoded_bytes();
    let read = |path: &Path, bytes: &mut Vec<u8>| -> std::io::Result<()> {
        bytes.clear();
        fs::File::open(path)?.read_to_end(bytes)?;
        Ok(())
    };
    fs::metadata("/proc/self/stat").context("checking the Prime process census")?;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let mut path = entry.path().join("stat");
        if let Err(error) = read(&path, &mut bytes) {
            if !process_disappeared(&error) && known.iter().any(|identity| identity.pid == pid) {
                return Err(error).with_context(|| format!("inspecting owned Prime process {pid}"));
            }
            continue;
        }
        let offset = bytes
            .windows(2)
            .rposition(|pair| pair == b") ")
            .context("invalid process stat")?;
        let tail = std::str::from_utf8(&bytes[offset + 2..])?;
        let mut fields = tail.split_ascii_whitespace();
        let state = fields.next().context("missing process state")?;
        let parent = fields.next().context("missing process parent")?.parse()?;
        let start = fields.nth(17).context("missing process start identity")?;
        if state == "Z" {
            continue;
        }
        let identity = super::PrimeProcessIdentity {
            pid,
            start_id: format!("proc:{start}"),
        };
        path.set_file_name("environ");
        let mut marked = read(&path, &mut bytes).is_ok()
            && bytes
                .split(|byte| *byte == 0)
                .any(|entry| entry == marker.as_bytes());
        if !marked {
            path.set_file_name("cmdline");
            marked = read(&path, &mut bytes).is_ok()
                && bytes.split(|byte| *byte == 0).any(|entry| entry == socket);
        }
        samples.push(super::PrimeProcessSample {
            identity,
            parent,
            marked,
        });
    }
    for identity in known {
        if !samples
            .iter()
            .any(|sample| sample.identity.pid == identity.pid)
        {
            let path = Path::new("/proc")
                .join(identity.pid.to_string())
                .join("stat");
            match read(&path, &mut bytes) {
                Ok(()) => anyhow::ensure!(
                    bytes
                        .windows(2)
                        .rposition(|pair| pair == b") ")
                        .is_some_and(|offset| bytes[offset + 2..].starts_with(b"Z ")),
                    "owned Prime process {} omitted from census",
                    identity.pid
                ),
                Err(error) if process_disappeared(&error) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspecting owned Prime process {}", identity.pid)
                    })
                }
            }
        }
    }
    super::select_stable_prime_processes(samples, known)
}

pub(super) fn sample_memory() -> super::metrics::MemorySample {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let total = parse_meminfo_field(&meminfo, "MemTotal").map(kib_to_bytes);
    let avail = parse_meminfo_field(&meminfo, "MemAvailable").map(kib_to_bytes);

    let psi_mem_some_avg10 =
        parse_psi_some_avg10(&fs::read_to_string("/proc/pressure/memory").unwrap_or_default());
    let psi_io_some_avg10 =
        parse_psi_some_avg10(&fs::read_to_string("/proc/pressure/io").unwrap_or_default());

    // Old kernels and WSL1 omit MemAvailable; report unknown rather than a false 100% used.
    let (total_bytes, available_bytes) = match (total, avail) {
        (Some(t), Some(a)) => (t, a),
        _ => (0, 0),
    };

    super::metrics::MemorySample {
        total_bytes,
        available_bytes,
        psi_mem_some_avg10,
        psi_io_some_avg10,
        macos_pressure_level: None,
    }
}

pub(super) fn sample_system() -> super::metrics::SystemReading {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let cpu = stat.lines().next().and_then(|line| {
        let values: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|value| value.parse().ok())
            .collect();
        (!values.is_empty()).then(|| {
            (
                values.iter().sum(),
                values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0),
            )
        })
    });
    let load = fs::read_to_string("/proc/loadavg").ok().and_then(|value| {
        let values: Vec<f64> = value
            .split_whitespace()
            .take(3)
            .filter_map(|part| part.parse().ok())
            .collect();
        (values.len() == 3).then(|| [values[0], values[1], values[2]])
    });
    let mem = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        mem.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let total = field("SwapTotal:");
    let free = field("SwapFree:");
    (cpu, None, load, (total, total.saturating_sub(free)))
}

pub(super) fn process_snapshot() -> Vec<super::metrics::ProcessRecord> {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let end = stat.rfind(')')?;
            let fields: Vec<&str> = stat[end + 2..].split_whitespace().collect();
            let ppid = fields.get(1)?.parse().ok()?;
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            let start_id = fields.get(19)?.parse().ok()?;
            let rss_pages: i64 = fields.get(21)?.parse().ok()?;
            Some(super::metrics::ProcessRecord {
                pid,
                ppid,
                start_id,
                rss_bytes: rss_pages.max(0) as u64 * page,
                cpu_seconds: (utime + stime) as f64 / hz,
            })
        })
        .collect()
}

fn kib_to_bytes(kib: u64) -> u64 {
    kib.saturating_mul(1024)
}

fn parse_meminfo_field(meminfo: &str, key: &str) -> Option<u64> {
    for line in meminfo.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

/// `None` when PSI is unavailable, never a false 0.0.
fn parse_psi_some_avg10(psi: &str) -> Option<f32> {
    for line in psi.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("some") {
            continue;
        }
        return fields.find_map(|kv| kv.strip_prefix("avg10=")?.parse().ok());
    }
    None
}

pub(super) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let stat_path = format!("/proc/{}/stat", shell_pid);
    let stat_content = fs::read_to_string(&stat_path).ok()?;

    let tpgid = parse_stat_field(&stat_content, 7)?;

    if tpgid <= 0 {
        return Some(shell_pid);
    }

    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

fn find_process_in_group(pgrp: u32) -> Option<u32> {
    let proc_dir = Path::new("/proc");
    if !proc_dir.exists() {
        return None;
    }

    // A process can exit mid-scan; skip it rather than abort to the shell pid.
    for entry in fs::read_dir(proc_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };

        let stat_path = entry.path().join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            continue;
        };

        if let Some(proc_pgrp) = parse_stat_field(&content, 4) {
            if proc_pgrp as u32 == pgrp {
                return Some(pid);
            }
        }
    }

    None
}

pub(super) fn foreground_arguments(shell_pid: u32) -> Vec<super::ProcessArguments> {
    use std::io::Read;
    let Some(foreground) = get_foreground_pid(shell_pid) else {
        return Vec::new();
    };
    let group = unsafe { libc::getpgid(foreground as i32) };
    if group < 0 {
        return Vec::new();
    }
    collect_pid_tree(foreground)
        .into_iter()
        .filter_map(|pid| {
            if unsafe { libc::getpgid(pid as i32) } != group {
                return None;
            }
            let mut bytes = Vec::new();
            fs::File::open(format!("/proc/{pid}/cmdline"))
                .ok()?
                .take(256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 256 * 1024 {
                return None;
            }
            let argv: Vec<String> = bytes
                .strip_suffix(&[0])
                .unwrap_or(&bytes)
                .split(|byte| *byte == 0)
                .map(|word| std::str::from_utf8(word).map(str::to_owned))
                .collect::<Result<_, _>>()
                .ok()?;

            let stdin = fs::read_link(format!("/proc/{pid}/fd/0")).ok();
            let stdin_is_tty = stdin.map(|path| {
                path.starts_with("/dev/pts")
                    || path == std::path::Path::new("/dev/tty")
                    || path == std::path::Path::new("/dev/console")
                    || path.to_string_lossy().starts_with("/dev/tty")
            });
            Some(super::ProcessArguments { argv, stdin_is_tty })
        })
        .collect()
}

pub(super) fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ppid = u32::try_from(parse_stat_field(&stat, 3)?).ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    Some((ppid, String::from_utf8_lossy(argv0).into_owned()))
}

/// `comm` (field 2) may contain spaces.
fn parse_stat_field(content: &str, field_idx: usize) -> Option<i64> {
    let close_paren = content.rfind(')')?;
    let after_comm = &content[close_paren + 2..]; // Skip ") "

    let adjusted_idx = field_idx.checked_sub(2)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(adjusted_idx)?.parse().ok()
}

pub(super) struct SystemdInhibitor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl SystemdInhibitor {
    pub(super) fn new() -> Self {
        Self {
            child: None,
            stdin: None,
        }
    }
}

impl super::SleepInhibit for SystemdInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        let mut child = match Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                "--mode=block",
                "--who=Agent of Empires",
                "--why=Active agent sessions",
                "cat",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::latch_sleep_inhibit_unavailable(
                    "systemd-inhibit not found; OS sleep will not be inhibited on this host",
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // `systemd-inhibit` holds the lock only while `cat` runs, which ends at stdin EOF.
        self.stdin = child.stdin.take();
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        // logind releases the lock when the holder dies by any cause.
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn is_held_alive(&mut self) -> bool {
        super::sleep_inhibit_child_held_alive(
            &mut self.child,
            "systemd-inhibit exited without taking the lock (no logind?); \
             OS sleep will not be inhibited on this host",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner_stat(state: u8, group: u32, threads: u32) -> Vec<u8> {
        let mut stat = format!("1234 (worker) {} 1 {group} 56", state as char).into_bytes();
        for _ in 0..13 {
            stat.extend_from_slice(b" 0");
        }
        stat.extend_from_slice(format!(" {threads} 0 987654").as_bytes());
        stat
    }

    #[test]
    fn launch_executable_recovers_only_deleted_paths_with_a_live_replacement() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("aoe");
        let deleted = directory.path().join("aoe (deleted)");
        assert!(launch_executable(deleted.clone()).is_err());
        fs::write(&installed, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(launch_executable(deleted.clone()).unwrap(), installed);
        fs::write(&deleted, "#!/bin/sh\n").unwrap();
        assert_eq!(launch_executable(deleted.clone()).unwrap(), deleted);
        fs::remove_file(&deleted).unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(launch_executable(deleted).is_err());
    }

    #[test]
    fn reaped_prime_stat_is_absence_but_other_read_errors_are_not() {
        use std::io::Read as _;
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut stat = fs::File::open(format!("/proc/{}/stat", child.id())).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        let error = stat.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert!(process_disappeared(&error));
        for (errno, gone) in [
            (libc::ENOENT, true),
            (libc::EACCES, false),
            (libc::EIO, false),
        ] {
            assert_eq!(
                process_disappeared(&std::io::Error::from_raw_os_error(errno)),
                gone
            );
        }
    }

    #[test]
    fn group_census_ignores_only_proven_foreign_permission_errors() {
        for (errno, owner, expected) in [
            (libc::EACCES, Some(1001), true),
            (libc::EPERM, Some(1001), true),
            (libc::EACCES, Some(1000), false),
            (libc::EPERM, None, false),
            (libc::EIO, Some(1001), false),
        ] {
            assert_eq!(
                foreign_process_permission_error(
                    &std::io::Error::from_raw_os_error(errno),
                    owner,
                    1000
                ),
                expected
            );
        }
    }

    #[test]
    fn runner_group_identity_census_matches_thread_settlement() {
        for (state, threads, expected) in [(b'S', 1, true), (b'Z', 1, false), (b'Z', 2, true)] {
            let stat = runner_stat(state, 55, threads);
            let identity = runner_group_member_identity(1234, &stat, 55).unwrap();
            assert_eq!(
                identity.is_some(),
                expected,
                "state={state}, threads={threads}"
            );
            if expected {
                assert_eq!(
                    identity.unwrap(),
                    super::super::PrimeProcessIdentity {
                        pid: 1234,
                        start_id: "proc:987654".into(),
                    }
                );
            }
            assert!(runner_group_member_identity(1234, &stat, 56)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    #[serial_test::serial]
    fn prime_quiescence_never_declares_an_uninspectable_owned_process_absent() {
        use std::io::{BufRead as _, Read as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;
        if std::env::var_os("AOE_PRIME_CENSUS_CHILD").is_some() {
            assert_eq!(
                unsafe {
                    libc::prctl(
                        libc::PR_SET_NAME,
                        b"\xffowned\0".as_ptr().cast::<libc::c_char>(),
                        0,
                        0,
                        0,
                    )
                },
                0
            );
            let signal = |message: &[u8]| {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(message).unwrap();
                stdout.flush().unwrap();
            };
            signal(b"warm-ready\n");
            std::io::stdin().read_exact(&mut [0]).unwrap();
            assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }, 0);
            signal(b"census-ready\n");
            std::io::stdin().read_exact(&mut [0]).unwrap();
            return;
        }
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        struct OwnedRuntime(crate::process::prime::PrimeRuntime);
        impl Drop for OwnedRuntime {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0.directory);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let program = root.path().join("native-shutdown");
        fs::write(&program, "#!/bin/sh\nprintf '%s' '{\"failed\":[]}'\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = OwnedRuntime(
            crate::process::prime::PrimeRuntime::new(program, root.path().join("agent"), None)
                .unwrap(),
        );
        runtime.0.prepare().unwrap();
        let cleanup = runtime.0.directory.join("cleanup");
        let original = runtime.0.directory.join("cleanup.original");
        fs::rename(&cleanup, &original).unwrap();
        std::os::unix::fs::symlink(root.path(), &cleanup).unwrap();
        assert!(runtime.0.cleanup().is_err());
        assert!(runtime.0.directory.is_dir());
        fs::remove_file(&cleanup).unwrap();
        fs::rename(original, cleanup).unwrap();
        let route = serde_json::to_string(&runtime.0).unwrap();
        let proof_path = crate::process::worker_registry::workers_dir()
            .unwrap()
            .join(format!(
                ".{}.prime-proof",
                runtime.0.directory.file_name().unwrap().to_str().unwrap()
            ));
        let mut receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&proof_path).unwrap()).unwrap();
        receipt["settled"] = serde_json::json!(true);
        fs::write(&proof_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        let name = format!(
            "{}::prime_quiescence_never_declares_an_uninspectable_owned_process_absent",
            module_path!().split_once("::").unwrap().1
        );
        let mut child = OwnedChild(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env("AOE_PRIME_CENSUS_CHILD", "1")
                .env("TMPDIR", &runtime.0.directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut reader = std::io::BufReader::new(child.0.stdout.take().unwrap());
        let mut ready = |expected: &str| {
            let mut line = String::new();
            loop {
                assert!(
                    reader.read_line(&mut line).unwrap() != 0,
                    "owned probe exited before readiness"
                );
                if line.trim() == expected {
                    break;
                }
                line.clear();
            }
        };
        ready("warm-ready");
        let stat = fs::read(format!("/proc/{}/stat", child.0.id())).unwrap();
        let offset = stat.windows(2).rposition(|pair| pair == b") ").unwrap();
        let start = std::str::from_utf8(&stat[offset + 2..])
            .unwrap()
            .split_ascii_whitespace()
            .nth(19)
            .unwrap();
        let known = super::super::PrimeProcessIdentity {
            pid: child.0.id(),
            start_id: format!("proc:{start}"),
        };
        assert!(runtime.0.cleanup().is_err());
        child.0.stdin.as_mut().unwrap().write_all(&[0]).unwrap();
        ready("census-ready");
        assert!(child.0.try_wait().unwrap().is_none());
        let marker = format!("TMPDIR={}", runtime.0.directory.display());
        let unbound = prime_processes(&marker, &runtime.0.socket(), &[]).unwrap();
        assert!(
            !unbound.iter().any(|identity| identity.pid == child.0.id()),
            "UID alone assigned foreign ownership"
        );
        let owned =
            prime_processes(&marker, &runtime.0.socket(), std::slice::from_ref(&known)).unwrap();
        assert!(
            owned.contains(&known),
            "unreadable owned environment was treated as absence"
        );
        let fresh: crate::process::prime::PrimeRuntime = serde_json::from_str(&route).unwrap();
        fresh.cleanup().unwrap();
        assert!(
            !child.0.wait().unwrap().success(),
            "the attested process must actually exit, not merely disappear from the census"
        );
        let fresh: crate::process::prime::PrimeRuntime = serde_json::from_str(&route).unwrap();
        fresh.cleanup().unwrap();
        assert!(!runtime.0.directory.exists());
    }

    #[test]
    #[serial_test::serial]
    fn fenced_purge_retains_a_live_thread_after_leader_exit() {
        use std::io::Write as _;
        use std::os::unix::process::CommandExt as _;
        use std::time::{Duration, Instant};

        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let temp = tempfile::TempDir::with_prefix_in("aoe-thread-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let source = temp.path().join("writer.c");
        let executable = temp.path().join("writer");
        let marker = temp.path().join("writing");
        fs::write(
            &source,
            r#"#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
static void *writer(void *path) {
    char command[32];
    while (fgets(command, sizeof(command), stdin)) {
        FILE *file = fopen(path, "w");
        if (!file) abort();
        fputs(command, file);
        fclose(file);
    }
    return NULL;
}
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    pthread_t thread;
    if (pthread_create(&thread, NULL, writer, argv[1])) return 3;
    pthread_exit(NULL);
}
"#,
        )
        .unwrap();
        let compiled = Command::new("cc")
            .arg("-pthread")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let mut child = ChildGuard(
            Command::new(&executable)
                .arg(&marker)
                .process_group(0)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let request_write = |child: &mut ChildGuard, sequence: &str| {
            write!(child.0.stdin.as_mut().unwrap(), "{sequence}").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while fs::read_to_string(&marker).ok().as_deref() != Some(sequence) {
                assert!(
                    Instant::now() < deadline,
                    "live thread did not acknowledge write"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        request_write(&mut child, "before\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap()
            .rsplit_once(')')
            .unwrap()
            .1
            .trim_start()
            .starts_with("Z ")
        {
            assert!(Instant::now() < deadline, "leader did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        use crate::process::worker_registry as registry;
        let session_id = "zombie-thread";
        let record = registry::WorkerRecord::new(
            session_id.into(),
            pid,
            registry::socket_path_for(session_id).unwrap(),
            "fixture".into(),
            "fixture".into(),
            temp.path().to_path_buf(),
            None,
            vec![],
            vec![],
            None,
            Some("profile".into()),
        );
        registry::save(&record).unwrap();
        registry::fence_for_purge(session_id, "profile", 1).unwrap();
        let stopped = registry::terminate_and_confirm_stopped(session_id, "profile", 1);
        request_write(&mut child, "after\n");
        assert!(
            stopped.is_err(),
            "purge confirmed stop while a thread could still write"
        );
        assert!(registry::load(session_id).unwrap().is_some());
        assert!(crate::process::runner_group_executing(pid).unwrap());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!crate::process::runner_group_executing(pid).unwrap());
        registry::terminate_and_confirm_stopped(session_id, "profile", 1).unwrap();
        assert!(registry::load(session_id).unwrap().is_none());
    }

    #[test]
    fn test_parse_stat_field() {
        let stat = "1234 (bash) S 1233 1234 1234 34816 1234 4194304 1234 0 0 0";

        assert_eq!(parse_stat_field(stat, 3), Some(1233)); // ppid
        assert_eq!(parse_stat_field(stat, 4), Some(1234)); // pgrp
        assert_eq!(parse_stat_field(stat, 7), Some(1234)); // tpgid
    }

    #[test]
    fn process_stat_parser_ignores_non_utf8_command_names() {
        let mut stat = b"1234 (bad".to_vec();
        stat.push(0xff);
        stat.extend_from_slice(b" name) S 1 55 56");
        for _ in 0..13 {
            stat.extend_from_slice(b" 0");
        }
        stat.extend_from_slice(b" 2 0 987654");

        let parsed = parse_kernel_process_stat(&stat).unwrap();
        assert_eq!(parsed.group, 55);
        assert_eq!(parsed.start_id, 987654);
        assert!(parsed.writing);
    }

    const MEMINFO: &str = "\
MemTotal:       32791036 kB
MemFree:         1234567 kB
MemAvailable:    9876543 kB
Cached:          5678901 kB
";

    #[test]
    fn test_parse_meminfo_field() {
        let cases = [
            ("MemTotal", Some(32791036)),
            ("MemAvailable", Some(9876543)),
            ("MemFree", Some(1234567)),
            ("Nonexistent", None),
            ("Mem", None),
        ];
        for (key, expected) in cases {
            assert_eq!(parse_meminfo_field(MEMINFO, key), expected, "{key}");
        }
    }

    #[test]
    fn test_parse_psi_some_avg10() {
        let psi = "\
some avg10=1.23 avg60=4.56 avg300=7.89 total=123456789
full avg10=0.10 avg60=0.20 avg300=0.30 total=42
";
        assert_eq!(parse_psi_some_avg10(psi), Some(1.23));
        assert_eq!(parse_psi_some_avg10(""), None);
        assert_eq!(parse_psi_some_avg10("full avg10=5.0 total=9"), None);
    }
}
