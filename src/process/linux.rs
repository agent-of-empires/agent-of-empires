//! Linux-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = false;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

pub(super) use super::unix::{
    configure_process_group, kill_process_group, terminate_process_group,
};
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

    #[test]
    fn test_parse_stat_field() {
        let stat = "1234 (bash) S 1233 1234 1234 34816 1234 4194304 1234 0 0 0";

        assert_eq!(parse_stat_field(stat, 3), Some(1233)); // ppid
        assert_eq!(parse_stat_field(stat, 4), Some(1234)); // pgrp
        assert_eq!(parse_stat_field(stat, 7), Some(1234)); // tpgid
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

pub(super) fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}
pub(super) fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

pub(crate) mod runtime_io {
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io::{self, Write};
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    pub(crate) const REGULAR: u32 = libc::S_IFREG;
    pub(crate) const DIRECTORY: u32 = libc::S_IFDIR;
    pub(crate) const SOCKET: u32 = libc::S_IFSOCK;
    pub(crate) const KIND_MASK: u32 = libc::S_IFMT;
    pub(crate) const STICKY: u32 = libc::S_ISVTX;
    pub(crate) enum DirectoryOpenFailure {
        Symlink,
        NotDirectory,
        Permission,
        Other,
    }
    pub(crate) fn directory_open_failure(error: &io::Error) -> DirectoryOpenFailure {
        match error.raw_os_error() {
            Some(libc::ELOOP) => DirectoryOpenFailure::Symlink,
            Some(libc::ENOTDIR) => DirectoryOpenFailure::NotDirectory,
            Some(libc::EACCES) | Some(libc::EPERM) => DirectoryOpenFailure::Permission,
            _ => DirectoryOpenFailure::Other,
        }
    }
    pub(crate) fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[derive(Clone, Copy)]
    pub(crate) struct Stat {
        pub st_mode: u32,
        pub st_uid: u32,
        pub st_dev: u64,
        pub st_ino: u64,
        pub st_nlink: libc::nlink_t,
    }
    fn stat_value(stat: libc::stat) -> Stat {
        Stat {
            st_mode: stat.st_mode,
            st_uid: stat.st_uid,
            st_dev: stat.st_dev,
            st_ino: stat.st_ino,
            st_nlink: stat.st_nlink,
        }
    }
    fn owned(fd: RawFd) -> io::Result<OwnedFd> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }
    fn name(value: &str) -> io::Result<CString> {
        CString::new(value).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }
    pub(crate) fn open_directory(path: &Path) -> io::Result<OwnedFd> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        owned(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
    }
    pub(crate) fn open_directory_at(
        dir: RawFd,
        name: &CStr,
        nofollow: bool,
    ) -> io::Result<OwnedFd> {
        let flags = libc::O_RDONLY
            | libc::O_DIRECTORY
            | libc::O_CLOEXEC
            | if nofollow { libc::O_NOFOLLOW } else { 0 };
        owned(unsafe { libc::openat(dir, name.as_ptr(), flags) })
    }
    pub(crate) fn open_readonly_at(dir: RawFd, name: &CStr) -> io::Result<File> {
        owned(unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
        .map(File::from)
    }
    pub(crate) fn open_lock_at(dir: RawFd, name: &CStr) -> io::Result<File> {
        owned(unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })
        .map(File::from)
    }
    pub(crate) fn stat_file(fd: RawFd) -> io::Result<Stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_value(unsafe { stat.assume_init() }))
    }
    pub(crate) fn stat_entry(dir: RawFd, value: &str) -> io::Result<Stat> {
        let value = name(value)?;
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                dir,
                value.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_value(unsafe { stat.assume_init() }))
    }
    pub(crate) fn set_file_mode(fd: RawFd, mode: u32) -> io::Result<()> {
        if unsafe { libc::fchmod(fd, mode) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(crate) fn set_entry_mode(dir: RawFd, value: &str, mode: u32) -> io::Result<()> {
        let value = name(value)?;
        if unsafe { libc::fchmodat(dir, value.as_ptr(), mode, 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(crate) fn unlink_entry(dir: RawFd, value: &str) -> io::Result<()> {
        let value = name(value)?;
        if unsafe { libc::unlinkat(dir, value.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    }
    pub(crate) fn sync_directory(dir: RawFd) {
        let _ = unsafe { libc::fsync(dir) };
    }
    pub(crate) fn anchored_path(dir: RawFd, child: &str) -> io::Result<PathBuf> {
        let base = PathBuf::from(format!("/proc/self/fd/{dir}"));
        if !base.exists() {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(base.join(child))
    }
    pub(crate) fn read_directory(dir: RawFd) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(anchored_path(dir, ".")?)
    }
    pub(crate) fn write_atomic(
        dir: RawFd,
        final_name: &str,
        temporary: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let temporary_name = name(temporary)?;
        let final_name = name(final_name)?;
        let fd = unsafe {
            libc::openat(
                dir,
                temporary_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        let mut file = File::from(owned(fd)?);
        let result = (|| {
            set_file_mode(file.as_raw_fd(), 0o600)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            if unsafe { libc::renameat(dir, temporary_name.as_ptr(), dir, final_name.as_ptr()) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        drop(file);
        if result.is_err() {
            let _ = unlink_entry(dir, temporary);
        }
        result
    }
    pub(crate) struct PeerCredentials {
        pub uid: u32,
        pub pid: u32,
    }
    pub(crate) fn peer_credentials(fd: RawFd) -> io::Result<PeerCredentials> {
        let mut credentials = MaybeUninit::<libc::ucred>::uninit();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let credentials = unsafe { credentials.assume_init() };
        Ok(PeerCredentials {
            uid: credentials.uid,
            pid: credentials.pid as u32,
        })
    }
    pub(crate) fn boot_id() -> Option<String> {
        super::boot_id()
    }
    pub(crate) enum ProcessStart {
        Ticks(String),
        Absent,
        Unprovable,
    }
    #[cfg(test)]
    static PROC_READ_UNPROVABLE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    #[cfg(test)]
    pub(crate) fn fail_next_proc_read() {
        PROC_READ_UNPROVABLE.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub(crate) fn process_start_ticks(pid: u32) -> ProcessStart {
        #[cfg(test)]
        if PROC_READ_UNPROVABLE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return ProcessStart::Unprovable;
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return ProcessStart::Absent,
            Err(_) => return ProcessStart::Unprovable,
        };
        let Some(close) = stat.rfind(')') else {
            return ProcessStart::Unprovable;
        };
        match stat[close + 1..]
            .split_whitespace()
            .nth(19)
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        {
            Some(ticks) => ProcessStart::Ticks(ticks.to_owned()),
            None => ProcessStart::Unprovable,
        }
    }
    pub(crate) fn process_start_identity(pid: u32) -> Option<String> {
        let boot = boot_id()?;
        let ProcessStart::Ticks(ticks) = process_start_ticks(pid) else {
            return None;
        };
        Some(format!("linux:v1:{boot}:{ticks}"))
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum AclError {
        NamedWrite,
        Unverifiable,
    }
    pub(crate) const ACL_VERSION: u32 = 2;
    pub(crate) const ACL_ENTRY_LEN: usize = 8;
    pub(crate) const ACL_USER_OBJ: u8 = 0x01;
    pub(crate) const ACL_USER: u8 = 0x02;
    pub(crate) const ACL_GROUP_OBJ: u8 = 0x04;
    pub(crate) const ACL_GROUP: u8 = 0x08;
    pub(crate) const ACL_MASK: u8 = 0x10;
    pub(crate) const ACL_OTHER: u8 = 0x20;
    pub(crate) const ACL_WRITE: u8 = 0x02;
    pub(crate) const ACL_PERMISSION_MASK: u8 = 0x07;
    pub(crate) fn validate_posix_acl(fd: RawFd) -> Result<(), AclError> {
        let name = c"system.posix_acl_access";
        let needed = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        if needed < 0 {
            return match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENODATA | libc::ENOTSUP) => Ok(()),
                _ => Err(AclError::Unverifiable),
            };
        }
        let mut value = vec![0u8; needed as usize];
        let actual =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        if actual < 0 {
            return Err(AclError::Unverifiable);
        }
        value.truncate(actual as usize);
        validate_acl_value(&value)
    }
    pub(crate) fn validate_acl_value(value: &[u8]) -> Result<(), AclError> {
        let header = value.get(..4).ok_or(AclError::Unverifiable)?;
        if u32::from_le_bytes(header.try_into().unwrap()) != ACL_VERSION {
            return Err(AclError::Unverifiable);
        }
        if value[4..].len() % ACL_ENTRY_LEN != 0 {
            return Err(AclError::Unverifiable);
        }
        for entry in value[4..].chunks_exact(ACL_ENTRY_LEN) {
            let tag = u16::from_le_bytes([entry[0], entry[1]]);
            let permissions = u16::from_le_bytes([entry[2], entry[3]]);
            let Ok(tag) = u8::try_from(tag) else {
                return Err(AclError::Unverifiable);
            };
            if !matches!(
                tag,
                ACL_USER_OBJ | ACL_USER | ACL_GROUP_OBJ | ACL_GROUP | ACL_MASK | ACL_OTHER
            ) || permissions & !u16::from(ACL_PERMISSION_MASK) != 0
            {
                return Err(AclError::Unverifiable);
            }
            if matches!(tag, ACL_USER | ACL_GROUP) && permissions & u16::from(ACL_WRITE) != 0 {
                return Err(AclError::NamedWrite);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_acl(dir: &Path, value: &[u8]) {
        let path = CString::new(dir.as_os_str().as_bytes()).expect("path");
        let name = c"system.posix_acl_access";
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(
            result,
            0,
            "set the access ACL: {}",
            io::Error::last_os_error()
        );
    }

    #[cfg(test)]
    pub(crate) fn read_acl(dir: &Path) -> Vec<u8> {
        let file = File::open(dir).expect("open the directory");
        let name = c"system.posix_acl_access";
        let needed =
            unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        assert!(needed > 0, "the directory must carry the access ACL");
        let mut value = vec![0u8; needed as usize];
        let read = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert!(read > 0, "read the access ACL back");
        value.truncate(read as usize);
        value
    }

    #[cfg(test)]
    pub(crate) struct Umask {
        previous: libc::mode_t,
        env_lock: Option<std::sync::MutexGuard<'static, ()>>,
    }

    #[cfg(test)]
    impl Umask {
        pub(crate) fn set(mask: libc::mode_t) -> Self {
            let env_lock = crate::test_env_lock::acquire_env_lock(|| {
                eprintln!(
                    "waiting for the environment lock: another test holds a process-wide state"
                )
            });
            Self {
                previous: unsafe { libc::umask(mask) },
                env_lock,
            }
        }
    }

    #[cfg(test)]
    impl Drop for Umask {
        fn drop(&mut self) {
            unsafe { libc::umask(self.previous) };
            crate::test_env_lock::release_env_lock(self.env_lock.is_some());
        }
    }
}
