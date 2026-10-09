//! Linux-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = false;
pub(super) const BOOTSTRAP_RECV_FLAGS: i32 = libc::MSG_CMSG_CLOEXEC;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

// Owned Create uses a genuinely held bootstrap and follows all native descendants.
pub(super) struct OwnedCreateRoot {
    pid: u32,
    handles: std::collections::BTreeMap<u32, Option<std::os::fd::OwnedFd>>,
}
impl OwnedCreateRoot {
    pub(super) fn prepare(birth: super::ProcessIncarnation) -> anyhow::Result<Self> {
        let pid = birth.pid;
        anyhow::ensure!(
            super::process_incarnation(pid)? == Some(birth) && birth.group == pid,
            "original held Linux bootstrap birth changed"
        );
        create_trace_begin(pid)?;
        anyhow::ensure!(
            super::process_incarnation(pid)? == Some(birth),
            "original held Linux bootstrap changed during trace admission"
        );
        Ok(Self {
            pid,
            handles: std::collections::BTreeMap::from([(pid, create_pidfd(pid)?)]),
        })
    }
    pub(super) fn release(&mut self) -> anyhow::Result<()> {
        create_trace_resume(self.pid, 0)
    }
    pub(super) fn retire(
        &mut self,
        child: &mut std::process::Child,
        cancel: &tokio_util::sync::CancellationToken,
        mut admit: impl FnMut(super::CreateObservation) -> anyhow::Result<()>,
    ) -> anyhow::Result<super::CreateRetirement> {
        use anyhow::Context;
        use std::collections::BTreeSet;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::ExitStatusExt;
        anyhow::ensure!(child.id() == self.pid, "original Create root was replaced");
        let mut live = BTreeSet::from([self.pid]);
        let handles = &mut self.handles;
        let mut observed = Vec::new();
        let mut root_status = None;
        let mut external = false;
        let mut scope_unproven = false;
        while !live.is_empty() {
            if cancel.is_cancelled() {
                for fd in handles.values().flatten() {
                    let result = unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd.as_raw_fd(),
                            libc::SIGKILL,
                            0,
                            0,
                        )
                    };
                    if result == -1
                        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
            }
            observed.clear();
            observed.extend(live.iter().copied());
            let mut made_progress = false;
            for &pid in &observed {
                let mut status = 0;
                let waited =
                    unsafe { libc::waitpid(pid as i32, &mut status, libc::__WALL | libc::WNOHANG) };
                if waited == 0 {
                    continue;
                }
                if waited == -1 {
                    return Err(std::io::Error::last_os_error().into());
                }
                made_progress = true;
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    live.remove(&pid);
                    if pid != self.pid {
                        handles.remove(&pid);
                    }
                    if pid == self.pid {
                        root_status = Some(status);
                    }
                    continue;
                }
                anyhow::ensure!(libc::WIFSTOPPED(status), "unrecognized native trace status");
                let event = status >> 16;
                let signal = libc::WSTOPSIG(status);
                if event == libc::PTRACE_EVENT_FORK
                    || event == libc::PTRACE_EVENT_VFORK
                    || event == libc::PTRACE_EVENT_CLONE
                {
                    let mut born: libc::c_ulong = 0;
                    create_ptrace(
                        libc::PTRACE_GETEVENTMSG,
                        pid,
                        0,
                        (&mut born as *mut libc::c_ulong) as usize,
                    )?;
                    let born = u32::try_from(born)?;
                    let birth = super::process_incarnation(born)?
                        .context("held descendant lacks actual kernel birth")?;
                    live.insert(born);
                    handles.insert(born, create_pidfd(born)?);
                    admit(super::CreateObservation::Birth(birth))?;
                } else if event == libc::PTRACE_EVENT_EXEC {
                    let mut former: libc::c_ulong = 0;
                    create_ptrace(
                        libc::PTRACE_GETEVENTMSG,
                        pid,
                        0,
                        (&mut former as *mut libc::c_ulong) as usize,
                    )?;
                    if former != 0 && former != pid as libc::c_ulong {
                        // Thread-group exec changes PID projection; never promote a fresh alias.
                        live.remove(&(former as u32));
                        handles.remove(&(former as u32));
                        if !scope_unproven {
                            admit(super::CreateObservation::ScopeUnproven)?;
                            scope_unproven = true;
                        }
                    }
                } else if signal == (libc::SIGTRAP | 0x80) {
                    let audit = create_audit_syscall(pid)?;
                    if audit.external && !external {
                        admit(super::CreateObservation::ExternalDomain)?;
                        external = true;
                    }
                    if audit.unproven && !scope_unproven {
                        admit(super::CreateObservation::ScopeUnproven)?;
                        scope_unproven = true;
                    }
                }
                if cancel.is_cancelled() {
                    create_ptrace(libc::PTRACE_KILL, pid, 0, 0)?;
                } else {
                    let delivery = if event != 0
                        || signal == (libc::SIGTRAP | 0x80)
                        || signal == libc::SIGSTOP
                    {
                        0
                    } else {
                        signal
                    };
                    create_trace_resume(pid, delivery)?;
                }
            }
            if !made_progress {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        // Keep this original producer until every original group member is gone.
        // Uncovered/foreign members are never signalled; observation failure is not absence.
        while process_group_has_live_members(self.pid)? {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let raw_status =
            root_status.context("original native root never returned its actual exit status")?;
        Ok(super::CreateRetirement {
            status: std::process::ExitStatus::from_raw(raw_status),
            raw_status,
            descendants_retired: !scope_unproven,
            group_retired: true,
            external_domain: external,
        })
    }
}
pub(super) fn hold_owned_create_bootstrap() -> anyhow::Result<()> {
    unsafe {
        if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == -1 || libc::raise(libc::SIGSTOP) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
fn create_ptrace(
    request: libc::c_uint,
    pid: u32,
    address: usize,
    data: usize,
) -> anyhow::Result<libc::c_long> {
    let value = unsafe {
        libc::ptrace(
            request,
            pid as libc::pid_t,
            address as *mut libc::c_void,
            data as *mut libc::c_void,
        )
    };
    if value == -1 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(value)
    }
}
fn create_trace_begin(pid: u32) -> anyhow::Result<()> {
    let mut status = 0;
    anyhow::ensure!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::__WALL) } == pid as i32
            && libc::WIFSTOPPED(status),
        "original native bootstrap was not held"
    );
    create_ptrace(
        libc::PTRACE_SETOPTIONS,
        pid,
        0,
        (libc::PTRACE_O_TRACEFORK
            | libc::PTRACE_O_TRACEVFORK
            | libc::PTRACE_O_TRACECLONE
            | libc::PTRACE_O_TRACEEXEC
            | libc::PTRACE_O_EXITKILL
            | libc::PTRACE_O_TRACESYSGOOD) as usize,
    )?;
    Ok(())
}
fn create_trace_resume(pid: u32, signal: i32) -> anyhow::Result<()> {
    create_ptrace(libc::PTRACE_SYSCALL, pid, 0, signal as usize).map(|_| ())
}
fn create_pidfd(pid: u32) -> anyhow::Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd >= 0 {
        return Ok(Some(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        }));
    }
    let error = std::io::Error::last_os_error();
    // Kernels without separate thread pidfds still retain real kernel ptrace ownership.
    if error.raw_os_error() == Some(libc::EINVAL) || error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    Err(error.into())
}
struct CreateSyscallAudit {
    external: bool,
    unproven: bool,
}
#[cfg(target_arch = "x86_64")]
fn create_audit_syscall(pid: u32) -> anyhow::Result<CreateSyscallAudit> {
    let mut info = [0u8; 128];
    let length = create_ptrace(0x420e, pid, info.len(), info.as_mut_ptr() as usize)? as usize;
    anyhow::ensure!(length >= 8, "native syscall ABI observation was incomplete");
    if info[0] != 1 {
        return Ok(CreateSyscallAudit {
            external: false,
            unproven: false,
        });
    }
    let arch = u32::from_ne_bytes(info[4..8].try_into().unwrap());
    if arch != 0xc000003e || length < 80 {
        return Ok(CreateSyscallAudit {
            external: false,
            unproven: true,
        });
    }
    let nr = u64::from_ne_bytes(info[24..32].try_into().unwrap());
    let argument =
        |index: usize| u64::from_ne_bytes(info[32 + index * 8..40 + index * 8].try_into().unwrap());
    if nr & 0x40000000 != 0 {
        return Ok(CreateSyscallAudit {
            external: false,
            unproven: true,
        });
    }
    let nr = nr as i64;
    let external = [
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_sendmsg,
        libc::SYS_sendto,
        libc::SYS_bpf,
        libc::SYS_io_uring_setup,
        libc::SYS_ptrace,
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_semop,
        libc::SYS_msgsnd,
        libc::SYS_mq_open,
        libc::SYS_mq_timedsend,
        libc::SYS_pidfd_getfd,
        libc::SYS_process_vm_writev,
    ]
    .contains(&nr)
        || ((nr == libc::SYS_kill || nr == libc::SYS_tkill) && argument(1) != 0)
        || (nr == libc::SYS_tgkill && argument(2) != 0)
        || (nr == libc::SYS_ioctl
            && ![libc::TCGETS, libc::TIOCGWINSZ, libc::FIONREAD].contains(&argument(1)));
    // clone3's shared userspace arguments can change after a peek; no fake complete coverage.
    Ok(CreateSyscallAudit {
        external,
        unproven: nr == libc::SYS_clone3
            || (nr == libc::SYS_clone && argument(0) & libc::CLONE_UNTRACED as u64 != 0),
    })
}
#[cfg(not(target_arch = "x86_64"))]
fn create_audit_syscall(_: u32) -> anyhow::Result<CreateSyscallAudit> {
    Ok(CreateSyscallAudit {
        external: false,
        unproven: true,
    })
}
pub(super) fn exec_owned_create(
    _program: &std::ffi::CString,
    executable: &std::fs::File,
    directory: &std::fs::File,
    argv: &[std::ffi::CString],
    env: &[std::ffi::CString],
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let mut argvp: Vec<_> = argv.iter().map(|v| v.as_ptr()).collect();
    argvp.push(std::ptr::null());
    let mut envp: Vec<_> = env.iter().map(|v| v.as_ptr()).collect();
    envp.push(std::ptr::null());
    let input = std::fs::File::open("/dev/null")?;
    unsafe {
        if libc::fchdir(directory.as_raw_fd()) != 0
            || libc::dup2(input.as_raw_fd(), libc::STDIN_FILENO) == -1
            || libc::fcntl(executable.as_raw_fd(), libc::F_SETFD, 0) == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
        libc::fexecve(executable.as_raw_fd(), argvp.as_ptr(), envp.as_ptr());
    }
    Err(std::io::Error::last_os_error().into())
}

pub(super) fn owned_create_descendant_traceable() -> bool {
    cfg!(target_arch = "x86_64")
}
pub(super) fn owned_create_anchor_path(
    _: &std::fs::File,
    slot: u32,
) -> anyhow::Result<(std::ffi::OsString, bool)> {
    Ok((format!("/proc/self/fd/{slot}").into(), false))
}
pub(super) fn install_owned_create_anchors(
    files: &[std::fs::File],
    slots: &[u32],
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    for (file, &slot) in files.iter().zip(slots) {
        if unsafe { libc::dup2(file.as_raw_fd(), slot as i32) } == -1
            || unsafe { libc::fcntl(slot as i32, libc::F_SETFD, 0) } == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

pub(super) fn is_process_group_alive(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    if super::worker::is_pid_alive_and_ours(pgid) && !is_terminated(pgid) {
        return true;
    }
    process_group_has_live_members(pgid).unwrap_or(true)
}

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

pub(super) fn process_namespace() -> std::io::Result<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    let status = fs::read_to_string("/proc/self/status")?;
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "PID namespace cannot be verified",
            )
        })?;
    let mut ids = line.split_whitespace();
    if ids.next().and_then(|id| id.parse::<u32>().ok()) != Some(std::process::id())
        || ids.next().is_some()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "procfs belongs to another PID namespace",
        ));
    }
    let namespace = fs::metadata("/proc/self/ns/pid")?;
    Ok([namespace.dev(), namespace.ino()])
}

// A hidden or unreadable process is not absent. Signal zero has no process effect.
pub(super) fn custodian_process_absent(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

pub(super) fn process_incarnation(pid: u32) -> std::io::Result<Option<super::ProcessIncarnation>> {
    let namespace = process_namespace()?;
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let end = stat.rfind(')').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "process stat has no command boundary",
        )
    })?;
    let mut fields = stat[end + 1..].split_whitespace();
    let group = fields
        .nth(2)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "process stat has no group")
        })?
        .parse()
        .map_err(std::io::Error::other)?;
    let start = fields
        .nth(16)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "process stat has no start time",
            )
        })?
        .parse()
        .map_err(std::io::Error::other)?;
    Ok(Some(super::ProcessIncarnation {
        pid,
        group,
        start: [start, 0],
        namespace,
    }))
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

/// Report non-zombie group members; failed observations do not prove absence.
pub(super) fn process_group_has_live_members(pgrp: u32) -> std::io::Result<bool> {
    use nix::{errno::Errno, sys::signal::killpg, unistd::Pid};

    match killpg(Pid::from_raw(pgrp as i32), None) {
        Ok(()) => {}
        Err(Errno::ESRCH) => return Ok(false),
        Err(error) => return Err(std::io::Error::from_raw_os_error(error as i32)),
    }
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid proc stat");
        // `comm` may contain spaces and parentheses, so the fields past it start
        // after the last one: state, ppid, pgrp, then the session id.
        let (_, fields) = stat.rsplit_once(')').ok_or_else(invalid)?;
        let mut fields = fields.split_whitespace();
        let state = fields.next().ok_or_else(invalid)?;
        let _parent = fields.next().ok_or_else(invalid)?;
        let group = fields.next().ok_or_else(invalid)?;
        if state != "Z" && group.parse::<u32>() == Ok(pgrp) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `pid` has already terminated, which for a child of this process means
/// it is waiting to be reaped rather than still running.
///
/// A zombie holds nothing: it cannot write, it holds no file descriptors open
/// on a checkout, and its process group is dead. Treating it as alive makes a
/// torn-down runner unprovable forever. The repo's own descendant wait already
/// uses this rule (`process::mod` test helper: "exited or a terminated zombie").
pub(super) fn is_terminated(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state letter follows `comm`, which may itself contain spaces and
    // parentheses, so anchor on the last `)`.
    let Some(close_paren) = stat.rfind(')') else {
        return false;
    };
    stat[close_paren + 1..].trim_start().starts_with('Z')
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
