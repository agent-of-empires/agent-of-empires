//! macOS-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = true;

pub(super) fn peer_pid_from_connected_socket(stream: &impl std::os::fd::AsFd) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};
    let pid = getsockopt(stream, LocalPeerPid).ok()?;
    (pid > 0).then_some(pid as u32)
}
use std::collections::HashMap;
use std::process::Command;
// Darwin retains the real original Child/group, but does not claim exhaustive
// descendant containment from a host CLI's exit or from a process-table guess.
pub(super) struct OwnedCreateRoot {
    birth: super::ProcessIncarnation,
}
impl OwnedCreateRoot {
    pub(super) fn prepare(birth: super::ProcessIncarnation) -> anyhow::Result<Self> {
        anyhow::ensure!(
            super::process_incarnation(birth.pid)? == Some(birth),
            "original held Darwin bootstrap birth changed"
        );
        anyhow::ensure!(
            birth.pid == birth.group,
            "original Darwin bootstrap is not its new group root"
        );
        Ok(Self { birth })
    }
    pub(super) fn release(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            super::process_incarnation(self.birth.pid)? == Some(self.birth),
            "held Darwin original changed before its private gate release"
        );
        Ok(())
    }
    pub(super) fn retire(
        &mut self,
        child: &mut std::process::Child,
        cancel: &tokio_util::sync::CancellationToken,
        _admit: impl FnMut(super::CreateObservation) -> anyhow::Result<()>,
    ) -> anyhow::Result<super::CreateRetirement> {
        use std::os::unix::process::ExitStatusExt;
        anyhow::ensure!(
            child.id() == self.birth.pid,
            "Darwin original Child was replaced"
        );
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if cancel.is_cancelled() {
                anyhow::ensure!(
                    super::process_incarnation(child.id())? == Some(self.birth),
                    "Darwin cancellation lost its original kernel birth"
                );
                child.kill()?;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        while process_group_has_live_members(self.birth.group)? {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(super::CreateRetirement {
            raw_status: status.clone().into_raw(),
            status,
            descendants_retired: false,
            group_retired: true,
            external_domain: false,
        })
    }
}
pub(super) fn hold_owned_create_bootstrap() -> anyhow::Result<()> {
    // The real private-channel read in the producer bootstrap is Darwin's gate.
    // Confirm the actual root/group before entering that held state.
    use anyhow::Context;
    let birth = super::process_incarnation(std::process::id())?
        .context("original Darwin bootstrap birth unavailable")?;
    anyhow::ensure!(
        birth.pid == birth.group,
        "Darwin native bootstrap changed its original group"
    );
    Ok(())
}
pub(super) fn exec_owned_create(
    program: &std::ffi::CString,
    executable: &std::fs::File,
    directory: &std::fs::File,
    argv: &[std::ffi::CString],
    env: &[std::ffi::CString],
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    // XNU has no fexecve. Retain and validate the real executable pin, execute
    // the sealed resolved pathname, and never turn this into containment proof.
    let metadata = std::fs::metadata(std::path::Path::new(std::ffi::OsStr::from_bytes(
        program.as_bytes(),
    )))?;
    anyhow::ensure!(
        crate::session::DirectoryIdentity::from_metadata(&metadata)
            == crate::session::DirectoryIdentity::from_metadata(&executable.metadata()?),
        "Darwin resolved executable changed before native exec"
    );
    let mut argvp: Vec<_> = argv.iter().map(|v| v.as_ptr()).collect();
    argvp.push(std::ptr::null());
    let mut envp: Vec<_> = env.iter().map(|v| v.as_ptr()).collect();
    envp.push(std::ptr::null());
    let input = std::fs::File::open("/dev/null")?;
    unsafe {
        if libc::fchdir(directory.as_raw_fd()) != 0
            || libc::dup2(input.as_raw_fd(), libc::STDIN_FILENO) == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
        libc::execve(program.as_ptr(), argvp.as_ptr(), envp.as_ptr());
    }
    Err(std::io::Error::last_os_error().into())
}

pub(super) fn owned_create_descendant_traceable() -> bool {
    false
}
pub(super) fn owned_create_anchor_path(
    file: &std::fs::File,
    _: u32,
) -> anyhow::Result<(std::ffi::OsString, bool)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    let mut path = [0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    let length = path
        .iter()
        .position(|&v| v == 0)
        .ok_or_else(|| anyhow::anyhow!("original Darwin FD path was not terminated"))?;
    let path = std::ffi::OsString::from_vec(path[..length].to_vec());
    anyhow::ensure!(
        crate::session::DirectoryIdentity::from_metadata(&std::fs::metadata(
            std::path::Path::new(&path)
        )?) == crate::session::DirectoryIdentity::from_metadata(&file.metadata()?),
        "Darwin F_GETPATH replaced its original role birth"
    );
    // XNU /dev/fd is dupfdopen, not a traversable directory capability; /.vol
    // also rebuilds a pathname. This is explicitly PathOnly, NEVER strong effect proof.
    Ok((path, true))
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

/// # Safety
/// `message` must point to live writable data and control buffers.
pub(super) unsafe fn receive_bootstrap_rights(
    channel: &std::os::unix::net::UnixStream,
    message: &mut libc::msghdr,
) -> std::io::Result<isize> {
    use std::os::fd::AsRawFd;

    let capacity = message.msg_controllen;
    loop {
        message.msg_controllen = capacity;
        // XNU peeks without creating FDs, but externalizes all rights before copyout.
        let received = unsafe { libc::recvmsg(channel.as_raw_fd(), message, libc::MSG_PEEK) };
        if received >= 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let truncated = message.msg_flags & libc::MSG_CTRUNC != 0 || message.msg_controllen > capacity;
    message.msg_controllen = capacity;
    message.msg_flags = 0;
    if truncated {
        channel.shutdown(std::net::Shutdown::Read)?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "original descriptor transfer was truncated",
        ));
    }
    loop {
        let received = unsafe { libc::recvmsg(channel.as_raw_fd(), message, 0) };
        if received >= 0 {
            return Ok(received);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
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
            // supplies live NUL-terminated names. RENAME_EXCL never replaces.
            nix::errno::Errno::result(unsafe {
                libc::renameatx_np(
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    destination_dir.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_EXCL,
                )
            })
            .map(|_| ())
        })
    })??;
    result.map_err(std::io::Error::from)
}

/// Run the child at background priority so a mass file delete does not saturate
/// `fseventsd` and stall the rest of the machine.
pub(super) fn throttle_child(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setpriority` is async-signal-safe and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
            Ok(())
        });
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

    let Ok(output) = Command::new("ps").args(["-o", "pid=,ppid=", "-A"]).output() else {
        return children_map;
    };

    if !output.status.success() {
        return children_map;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(child_pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                children_map.entry(ppid).or_default().push(child_pid);
            }
        }
    }

    children_map
}

/// `-E` appends each owned process's environment; if `ps` rejects it, every candidate is `false`.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
) -> Vec<bool> {
    let n = env_needles.len();
    let mut found = vec![false; n];
    let Ok(output) = Command::new("ps")
        .args(["-A", "-ww", "-E", "-o", "command="])
        .output()
    else {
        return found;
    };
    if !output.status.success() {
        return found;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_position = (!env_needles[i].is_empty())
                .then(|| tokens.iter().position(|token| *token == env_needles[i]))
                .flatten();
            let env_hit = env_position.is_some();
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && line.contains(s));
            let command_end = env_position.unwrap_or(tokens.len());
            let executable_hit = executable_needles[i].as_deref().is_some_and(|needle| {
                !needle.is_empty()
                    && tokens[..command_end].iter().any(|token| {
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
            }
        }
    }
    found
}

pub(super) fn sample_memory() -> super::metrics::MemorySample {
    // A vm_stat failure alone would otherwise read as a false 100% used.
    let (total_bytes, available_bytes) = match (
        sysctl_u64("hw.memsize"),
        read_vm_stat().and_then(|s| parse_vm_stat_available(&s)),
    ) {
        (Some(total), Some(available)) => (total, available),
        _ => (0, 0),
    };

    super::metrics::MemorySample {
        total_bytes,
        available_bytes,
        psi_mem_some_avg10: None,
        psi_io_some_avg10: None,
        macos_pressure_level: sysctl_u64("kern.memorystatus_vm_pressure_level").map(|v| v as u8),
    }
}

pub(super) fn sample_system() -> super::metrics::SystemReading {
    let cpu = Command::new("ps")
        .args(["-A", "-o", "%cpu="])
        .output()
        .ok()
        .map(|output| {
            let total_percent: f64 = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<f64>().ok())
                .sum();
            let cpus = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1);
            (total_percent / 100.0 / cpus as f64).clamp(0.0, 1.0)
        });
    let load = sysctl_string("vm.loadavg").and_then(|value| {
        let values: Vec<f64> = value
            .replace(['{', '}'], "")
            .split_whitespace()
            .filter_map(|part| part.parse().ok())
            .collect();
        (values.len() >= 3).then(|| [values[0], values[1], values[2]])
    });
    let swap = sysctl_string("vm.swapusage")
        .map(|value| parse_swap(&value))
        .unwrap_or((0, 0));
    (None, cpu, load, swap)
}

pub(super) fn process_snapshot() -> Vec<super::metrics::ProcessRecord> {
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time=,lstart="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_record)
        .collect()
}

fn parse_process_record(line: &str) -> Option<super::metrics::ProcessRecord> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let started_at = fields.get(4..)?;
    if started_at.is_empty() {
        return None;
    }
    Some(super::metrics::ProcessRecord {
        pid: fields.first()?.parse().ok()?,
        ppid: fields.get(1)?.parse().ok()?,
        rss_bytes: fields.get(2)?.parse::<u64>().ok()?.saturating_mul(1024),
        cpu_seconds: parse_ps_time(fields.get(3)?)?,
        start_id: stable_process_start_id(started_at),
    })
}

fn stable_process_start_id(fields: &[&str]) -> u64 {
    fields
        .iter()
        .flat_map(|field| field.bytes().chain(std::iter::once(0)))
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn parse_swap(value: &str) -> (u64, u64) {
    let parse = |label: &str| {
        let raw = value
            .split_whitespace()
            .skip_while(|part| *part != label)
            .nth(2)?;
        let (number, multiplier) = if let Some(number) = raw.strip_suffix('G') {
            (number, 1u64 << 30)
        } else if let Some(number) = raw.strip_suffix('M') {
            (number, 1u64 << 20)
        } else if let Some(number) = raw.strip_suffix('K') {
            (number, 1u64 << 10)
        } else {
            (raw, 1)
        };
        Some((number.parse::<f64>().ok()? * multiplier as f64) as u64)
    };
    (parse("total").unwrap_or(0), parse("used").unwrap_or(0))
}

fn parse_ps_time(value: &str) -> Option<f64> {
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<f64>().ok()?, clock)
    } else {
        (0.0, value)
    };
    let parts: Vec<f64> = clock
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes * 60.0 + seconds,
        [hours, minutes, seconds] => hours * 3600.0 + minutes * 60.0 + seconds,
        _ => return None,
    };
    Some(days * 86_400.0 + seconds)
}

fn sysctl_string(key: &str) -> Option<String> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn sysctl_u64(key: &str) -> Option<u64> {
    sysctl_string(key)?.parse().ok()
}

fn read_vm_stat() -> Option<String> {
    let out = Command::new("vm_stat").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// (free + inactive) pages; an approximation, so the band uses the native pressure level.
fn parse_vm_stat_available(vm_stat: &str) -> Option<u64> {
    let page_size = parse_vm_stat_page_size(vm_stat)?;
    let free = parse_vm_stat_pages(vm_stat, "Pages free")?;
    let inactive = parse_vm_stat_pages(vm_stat, "Pages inactive")?;
    Some((free + inactive).saturating_mul(page_size))
}

fn parse_vm_stat_page_size(vm_stat: &str) -> Option<u64> {
    let line = vm_stat.lines().next()?;
    let after = line.split("page size of").nth(1)?;
    after.split_whitespace().next()?.parse().ok()
}

fn parse_vm_stat_pages(vm_stat: &str, key: &str) -> Option<u64> {
    for line in vm_stat.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.trim().trim_end_matches('.').parse().ok();
    }
    None
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    const VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                              123456.
Pages active:                            654321.
Pages inactive:                          200000.
Pages speculative:                        10000.
Pages wired down:                        300000.
";

    #[test]
    fn test_parse_vm_stat_available() {
        assert_eq!(parse_vm_stat_page_size(VM_STAT), Some(16384));
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages free"), Some(123456));
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages inactive"), Some(200000));
        assert_eq!(
            parse_vm_stat_available(VM_STAT),
            Some((123456 + 200000) * 16384)
        );
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages nonexistent"), None);
    }

    #[test]
    fn process_record_parses_stable_identity_and_cpu_time() {
        let time_cases = [
            ("03:04", Some(184.0)),
            ("02:03:04", Some(7_384.0)),
            ("1-02:03:04", Some(93_784.0)),
            ("1-02:x:04", None),
            ("1-02:03:bad", None),
        ];
        for (value, expected) in time_cases {
            assert_eq!(parse_ps_time(value), expected, "CPU time {value}");
        }

        let line = "42 1 512 1-02:03:04 Thu Aug 13 12:34:56 2026";
        let record = parse_process_record(line).unwrap();
        assert_eq!(record.cpu_seconds, 93_784.0);
        assert_ne!(record.start_id, 0);
        assert_eq!(
            record.start_id,
            parse_process_record(line).unwrap().start_id
        );
    }
}

// A hidden or unreadable process is not absent. Signal zero has no process effect.
pub(super) fn custodian_process_absent(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

pub(super) fn process_incarnation(pid: u32) -> std::io::Result<Option<super::ProcessIncarnation>> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let written = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if written == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    if written != size as i32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "incomplete process incarnation",
        ));
    }
    // PROC_PIDTBSDINFO returned the complete fixed-size structure.
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "process identity differs",
        ));
    }
    Ok(Some(super::ProcessIncarnation {
        pid,
        group: info.pbi_pgid,
        start: [info.pbi_start_tvsec, info.pbi_start_tvusec],
        namespace: [0, 0],
    }))
}

pub(super) fn process_namespace() -> std::io::Result<[u64; 2]> {
    Ok([0, 0])
}

/// The session UUID is independent of clock steps.
pub(super) fn boot_id() -> Option<String> {
    let out = Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Report non-zombie group members. Probe failures do not prove absence.
pub(super) fn process_group_has_live_members(pgrp: u32) -> std::io::Result<bool> {
    use nix::{errno::Errno, sys::signal::killpg, unistd::Pid};

    match killpg(Pid::from_raw(pgrp as i32), None) {
        // Darwin excludes zombies from its permission check.
        Ok(()) | Err(Errno::EPERM) => {}
        Err(Errno::ESRCH) => return Ok(false),
        Err(error) => return Err(std::io::Error::from_raw_os_error(error as i32)),
    }
    // Match the explicit group column, not BSD ps -g.
    let output = Command::new("/bin/ps")
        .args(["-o", "pid=,pgid=,state=", "-A"])
        .output()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "ps exited with {}",
            output.status
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _pid = fields.next()?;
            let group = fields.next()?;
            let state = fields.next()?;
            (group.parse::<u32>() == Ok(pgrp)).then_some(state)
        })
        // BSD state suffixes do not change the leading zombie state.
        .any(|state| !state.starts_with('Z')))
}

/// Darwin cannot reap an unrelated process, so query its state instead.
pub(super) fn is_terminated(pid: u32) -> bool {
    match process_state(pid) {
        Ok(state) => state.starts_with('Z'),
        // Neither query failure nor an absent row proves termination.
        _ => false,
    }
}

/// The BSD state field of one process. BSD `ps` prints the state letter
/// followed by its flags, so a zombie reads `ZN` rather than `Z`.
fn process_state(pid: u32) -> Result<String, std::io::Error> {
    let output = Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "ps -p {pid} exited with {}",
            output.status
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| std::io::Error::other(format!("ps -p {pid} printed no state")))
}

#[cfg(test)]
mod termination_tests {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        }
    }

    struct ZombieParent {
        pid: libc::pid_t,
        release: Option<OwnedFd>,
    }

    impl Drop for ZombieParent {
        fn drop(&mut self) {
            drop(self.release.take());
            while unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) } < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
        }
    }

    #[test]
    fn a_zombie_observed_by_another_parent_is_terminated() {
        use std::io::Read;

        let (ready_read, ready_write) = pipe();
        let (release_read, release_write) = pipe();
        let parent_pid = unsafe { libc::fork() };
        assert!(parent_pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if parent_pid == 0 {
            // Only async-signal-safe syscalls run in either post-fork child.
            unsafe {
                libc::close(ready_read.as_raw_fd());
                libc::close(release_write.as_raw_fd());
                let zombie_pid = libc::fork();
                if zombie_pid < 0 {
                    libc::_exit(2);
                }
                if zombie_pid == 0 {
                    libc::_exit(0);
                }
                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
                while libc::waitid(
                    libc::P_PID,
                    zombie_pid as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                ) < 0
                {
                    if *libc::__error() != libc::EINTR {
                        libc::_exit(3);
                    }
                }
                let bytes = zombie_pid.to_ne_bytes();
                if libc::write(ready_write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                    != bytes.len() as libc::ssize_t
                {
                    libc::_exit(4);
                }
                let mut byte = 0_u8;
                while libc::read(release_read.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) < 0 {
                    if *libc::__error() != libc::EINTR {
                        break;
                    }
                }
                while libc::waitpid(zombie_pid, std::ptr::null_mut(), 0) < 0 {
                    if *libc::__error() != libc::EINTR {
                        libc::_exit(5);
                    }
                }
                libc::_exit(0);
            }
        }
        let _parent = ZombieParent {
            pid: parent_pid,
            release: Some(release_write),
        };
        drop(ready_write);
        drop(release_read);
        let mut ready = libc::pollfd {
            fd: ready_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut ready, 1, 5_000) }, 1);
        let mut bytes = [0; std::mem::size_of::<libc::pid_t>()];
        std::fs::File::from(ready_read)
            .read_exact(&mut bytes)
            .unwrap();
        let zombie_pid = libc::pid_t::from_ne_bytes(bytes);
        assert_eq!(
            unsafe { libc::waitpid(zombie_pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(super::is_terminated(zombie_pid as u32));
    }
}
pub(super) fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    let output = Command::new("ps")
        .args(["-o", "ppid=,args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, fields.next().unwrap_or_default().to_string()))
}

pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "tpgid=", "-p", &shell_pid.to_string()])
        .output()
        .ok()?;

    if !output.status.success() {
        return Some(shell_pid);
    }

    let tpgid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;

    if tpgid <= 0 {
        return Some(shell_pid);
    }

    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

fn find_process_in_group(pgrp: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "pid=,pgid=", "-A"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(pid), Ok(proc_pgrp)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                if proc_pgrp == pgrp {
                    return Some(pid);
                }
            }
        }
    }

    None
}

pub(super) struct CaffeinateInhibitor {
    child: Option<std::process::Child>,
}

impl CaffeinateInhibitor {
    pub(super) fn new() -> Self {
        Self { child: None }
    }
}

impl super::SleepInhibit for CaffeinateInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        // `-w <daemon_pid>` releases the assertion even when the daemon dies without `Drop`.
        let child = match Command::new("caffeinate")
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::latch_sleep_inhibit_unavailable(
                    "caffeinate not found; OS sleep will not be inhibited on this host",
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn is_held_alive(&mut self) -> bool {
        super::sleep_inhibit_child_held_alive(
            &mut self.child,
            "caffeinate exited unexpectedly; OS sleep will not be inhibited on this host",
        )
    }
}
